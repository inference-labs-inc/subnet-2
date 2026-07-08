use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use rand::Rng;
use sn2_types::{
    DISCONNECT_SKIPLIST_TEMPOS, RSV_EXPECTED_SUBS_PER_TEMPO, VERIFICATION_COLDSTART_BLOCKS,
    VERIFICATION_COLDSTART_RETENTION_BLOCKS, VERIFICATION_HISTORY_CAP,
    VERIFICATION_SAMPLES_PER_TEMPO, VERIFICATION_SKIPLIST_TEMPOS, VERIFICATION_STRIKES_REQUIRED,
    VERIFICATION_STRIKES_WINDOW_BLOCKS,
};
use tracing::{info, warn};

pub struct RsvManager {
    skiplist: HashMap<String, u64>,
    strikes: HashMap<String, VecDeque<u64>>,
    coldstart: HashMap<String, u64>,
    last_seen: HashMap<String, u64>,
    persistence_path: Option<PathBuf>,
}

impl RsvManager {
    pub fn new_with_persistence(path: PathBuf) -> Self {
        let mut mgr = Self {
            skiplist: HashMap::new(),
            strikes: HashMap::new(),
            coldstart: HashMap::new(),
            last_seen: HashMap::new(),
            persistence_path: Some(path),
        };
        mgr.load();
        mgr
    }

    pub fn is_skiplisted(&self, hotkey: &str, current_block: u64) -> bool {
        self.skiplist
            .get(hotkey)
            .is_some_and(|&until| current_block < until)
    }

    pub fn is_in_coldstart(&self, hotkey: &str, current_block: u64) -> bool {
        match self.coldstart.get(hotkey) {
            Some(&first_seen) => {
                current_block.saturating_sub(first_seen) < VERIFICATION_COLDSTART_BLOCKS
            }
            None => true,
        }
    }

    pub fn observe(&mut self, hotkey: &str, current_block: u64) {
        self.coldstart
            .entry(hotkey.to_string())
            .or_insert(current_block);
        self.last_seen.insert(hotkey.to_string(), current_block);
    }

    pub fn should_sample(
        &mut self,
        _hotkey: &str,
        _current_block: u64,
        _blocks_per_tempo: u64,
    ) -> bool {
        let mut rng = rand::rng();
        let roll: u64 = rng.random_range(0..RSV_EXPECTED_SUBS_PER_TEMPO);
        roll < VERIFICATION_SAMPLES_PER_TEMPO
    }

    pub fn skiplist_disconnect(&mut self, hotkey: &str, current_block: u64, blocks_per_tempo: u64) {
        let bpt = if blocks_per_tempo == 0 {
            360
        } else {
            blocks_per_tempo
        };
        let until = current_block + DISCONNECT_SKIPLIST_TEMPOS * bpt;
        let entry = self.skiplist.entry(hotkey.to_string()).or_insert(0);
        if until > *entry {
            *entry = until;
            warn!(
                hotkey = %hotkey,
                until_block = until,
                "rsv: miner not connected, skiplisted for one epoch"
            );
        }
    }

    pub fn record_strike(
        &mut self,
        hotkey: &str,
        current_block: u64,
        blocks_per_tempo: u64,
    ) -> bool {
        let entry = self.strikes.entry(hotkey.to_string()).or_default();
        entry.push_back(current_block);
        let cutoff = current_block.saturating_sub(VERIFICATION_STRIKES_WINDOW_BLOCKS);
        while let Some(&front) = entry.front() {
            if front < cutoff {
                entry.pop_front();
            } else {
                break;
            }
        }
        if entry.len() as u32 >= VERIFICATION_STRIKES_REQUIRED {
            let bpt = if blocks_per_tempo == 0 {
                360
            } else {
                blocks_per_tempo
            };
            let until = current_block + VERIFICATION_SKIPLIST_TEMPOS * bpt;
            self.skiplist.insert(hotkey.to_string(), until);
            self.strikes.remove(hotkey);
            warn!(
                hotkey = %hotkey,
                until_block = until,
                "rsv: strike threshold reached, miner skiplisted"
            );
            true
        } else {
            false
        }
    }

    pub fn prune_expired(&mut self, current_block: u64, blocks_per_tempo: u64) {
        self.skiplist.retain(|_, until| *until > current_block);

        let strikes_cutoff = current_block.saturating_sub(VERIFICATION_STRIKES_WINDOW_BLOCKS);
        let coldstart_cutoff =
            current_block.saturating_sub(VERIFICATION_COLDSTART_RETENTION_BLOCKS);

        let strikes_stale: Vec<String> = self
            .last_seen
            .iter()
            .filter(|(_, &seen)| seen < strikes_cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        for hotkey in &strikes_stale {
            self.strikes.remove(hotkey);
        }

        let coldstart_stale: Vec<String> = self
            .last_seen
            .iter()
            .filter(|(_, &seen)| seen < coldstart_cutoff)
            .map(|(k, _)| k.clone())
            .collect();
        for hotkey in &coldstart_stale {
            self.coldstart.remove(hotkey);
            self.last_seen.remove(hotkey);
        }

        if self.last_seen.len() > VERIFICATION_HISTORY_CAP {
            let mut by_age: Vec<(String, u64)> = self
                .last_seen
                .iter()
                .map(|(k, &v)| (k.clone(), v))
                .collect();
            by_age.sort_by_key(|(_, v)| *v);
            let drop_n = self.last_seen.len() - VERIFICATION_HISTORY_CAP;
            for (hotkey, _) in by_age.into_iter().take(drop_n) {
                self.coldstart.remove(&hotkey);
                self.strikes.remove(&hotkey);
                self.last_seen.remove(&hotkey);
            }
        }

        let _ = blocks_per_tempo;
    }

    pub fn save(&self) {
        let path = match &self.persistence_path {
            Some(p) => p,
            None => return,
        };
        let skiplist_json: serde_json::Map<String, serde_json::Value> = self
            .skiplist
            .iter()
            .map(|(hk, until)| (hk.clone(), serde_json::json!(*until)))
            .collect();
        let strikes_json: serde_json::Map<String, serde_json::Value> = self
            .strikes
            .iter()
            .map(|(hk, deque)| {
                (
                    hk.clone(),
                    serde_json::Value::Array(deque.iter().map(|b| serde_json::json!(*b)).collect()),
                )
            })
            .collect();
        let coldstart_json: serde_json::Map<String, serde_json::Value> = self
            .coldstart
            .iter()
            .map(|(hk, first)| (hk.clone(), serde_json::json!(*first)))
            .collect();
        let last_seen_json: serde_json::Map<String, serde_json::Value> = self
            .last_seen
            .iter()
            .map(|(hk, seen)| (hk.clone(), serde_json::json!(*seen)))
            .collect();
        let data = serde_json::json!({
            "version": 2,
            "skiplist": skiplist_json,
            "strikes": strikes_json,
            "coldstart": coldstart_json,
            "last_seen": last_seen_json,
        });
        match serde_json::to_string(&data) {
            Ok(json) => {
                if let Err(e) = sn2_types::atomic_write_json(path, json.as_bytes()) {
                    warn!(error = %e, "saving rsv state");
                }
            }
            Err(e) => warn!(error = %e, "serializing rsv state"),
        }
    }

    fn load(&mut self) {
        let path = match &self.persistence_path {
            Some(p) => p,
            None => return,
        };
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "rsv load: read failed, preserving in-memory state");
                return;
            }
        };
        let parsed: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "rsv load: parse failed, starting fresh");
                return;
            }
        };
        let version = parsed.get("version").and_then(|v| v.as_u64()).unwrap_or(1);
        if version < 2 {
            info!("rsv load: legacy uid-keyed state detected, discarding and starting fresh");
            return;
        }
        if let Some(map) = parsed.get("skiplist").and_then(|v| v.as_object()) {
            for (k, v) in map {
                if let Some(until) = v.as_u64() {
                    self.skiplist.insert(k.clone(), until);
                }
            }
        }
        if let Some(map) = parsed.get("strikes").and_then(|v| v.as_object()) {
            for (k, v) in map {
                let arr = match v.as_array() {
                    Some(a) => a,
                    None => continue,
                };
                let deque: VecDeque<u64> = arr.iter().filter_map(|x| x.as_u64()).collect();
                if !deque.is_empty() {
                    self.strikes.insert(k.clone(), deque);
                }
            }
        }
        if let Some(map) = parsed.get("coldstart").and_then(|v| v.as_object()) {
            for (k, v) in map {
                if let Some(first) = v.as_u64() {
                    self.coldstart.insert(k.clone(), first);
                }
            }
        }
        if let Some(map) = parsed.get("last_seen").and_then(|v| v.as_object()) {
            for (k, v) in map {
                if let Some(seen) = v.as_u64() {
                    self.last_seen.insert(k.clone(), seen);
                }
            }
        }
        info!(
            skiplisted = self.skiplist.len(),
            tracked_strikes = self.strikes.len(),
            observed = self.coldstart.len(),
            last_seen = self.last_seen.len(),
            "rsv state loaded"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sn2_rsv_test_{}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            suffix
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("rsv.json")
    }

    fn fresh() -> RsvManager {
        RsvManager {
            skiplist: HashMap::new(),
            strikes: HashMap::new(),
            coldstart: HashMap::new(),
            last_seen: HashMap::new(),
            persistence_path: None,
        }
    }

    #[test]
    fn coldstart_gates_new_hotkeys() {
        let mut mgr = fresh();
        mgr.observe("hk1", 1000);
        assert!(mgr.is_in_coldstart("hk1", 2000));
        assert!(!mgr.is_in_coldstart("hk1", 3000));
    }

    #[test]
    fn coldstart_unknown_hotkey_is_in_coldstart() {
        let mgr = fresh();
        assert!(mgr.is_in_coldstart("unknown", 1_000_000));
    }

    #[test]
    fn observe_does_not_overwrite_first_seen() {
        let mut mgr = fresh();
        mgr.observe("hk1", 1000);
        mgr.observe("hk1", 2000);
        assert_eq!(mgr.coldstart.get("hk1").copied(), Some(1000));
        assert_eq!(mgr.last_seen.get("hk1").copied(), Some(2000));
    }

    #[test]
    fn disconnect_skiplists_for_one_epoch_then_expires() {
        let mut mgr = fresh();
        let bpt = 360;
        mgr.skiplist_disconnect("hk1", 1000, bpt);
        assert!(mgr.is_skiplisted("hk1", 1000));
        assert!(mgr.is_skiplisted("hk1", 1000 + bpt - 1));
        assert!(!mgr.is_skiplisted("hk1", 1000 + bpt));
    }

    #[test]
    fn disconnect_never_shortens_a_longer_skiplist() {
        let mut mgr = fresh();
        // A verification skiplist runs many tempos; a later disconnect must not
        // cut it short.
        mgr.record_strike("hk1", 100, 360);
        assert!(mgr.is_skiplisted("hk1", 100 + 360));
        mgr.skiplist_disconnect("hk1", 200, 360);
        assert!(
            mgr.is_skiplisted("hk1", 100 + 360),
            "disconnect must not shorten the longer verification skiplist"
        );
    }

    #[test]
    fn record_strike_at_threshold_skiplists() {
        let mut mgr = fresh();
        for i in 0..VERIFICATION_STRIKES_REQUIRED {
            let triggered = mgr.record_strike("hk1", 100 + i as u64, 360);
            if i + 1 < VERIFICATION_STRIKES_REQUIRED {
                assert!(!triggered);
            } else {
                assert!(triggered);
            }
        }
        let block = 100 + (VERIFICATION_STRIKES_REQUIRED as u64) - 1;
        assert!(mgr.is_skiplisted("hk1", block));
        assert!(!mgr.strikes.contains_key("hk1"));
        let until = mgr.skiplist.get("hk1").copied().unwrap();
        assert_eq!(until, block + VERIFICATION_SKIPLIST_TEMPOS * 360);
    }

    #[test]
    fn prune_expired_drops_past_skiplist() {
        let mut mgr = fresh();
        mgr.skiplist.insert("hk1".to_string(), 200);
        mgr.skiplist.insert("hk2".to_string(), 5_000_000);
        mgr.prune_expired(300, 360);
        assert!(!mgr.skiplist.contains_key("hk1"));
        assert!(mgr.skiplist.contains_key("hk2"));
    }

    #[test]
    fn save_load_round_trip() {
        let path = temp_path("roundtrip");
        let mut mgr = RsvManager::new_with_persistence(path.clone());
        mgr.observe("hk_a", 500);
        mgr.skiplist.insert("hk_b".to_string(), 9000);
        mgr.strikes
            .entry("hk_c".to_string())
            .or_default()
            .push_back(123);
        mgr.last_seen.insert("hk_c".to_string(), 123);
        mgr.save();

        let loaded = RsvManager::new_with_persistence(path.clone());
        assert_eq!(loaded.coldstart.get("hk_a").copied(), Some(500));
        assert_eq!(loaded.skiplist.get("hk_b").copied(), Some(9000));
        assert_eq!(
            loaded.strikes.get("hk_c").unwrap().front().copied(),
            Some(123)
        );
        assert_eq!(loaded.last_seen.get("hk_a").copied(), Some(500));

        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn legacy_state_discarded_on_load() {
        let path = temp_path("legacy");
        let legacy = serde_json::json!({
            "skiplist": { "1": 9000 },
            "strikes": { "2": [50] },
            "coldstart": { "3": 100 },
        });
        std::fs::write(&path, legacy.to_string()).unwrap();
        let loaded = RsvManager::new_with_persistence(path.clone());
        assert!(loaded.skiplist.is_empty());
        assert!(loaded.strikes.is_empty());
        assert!(loaded.coldstart.is_empty());

        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn should_sample_rate_is_volume_invariant() {
        let mut mgr = fresh();
        let mut hits = 0;
        let trials = 100_000;
        for _ in 0..trials {
            if mgr.should_sample("hk1", 50, 360) {
                hits += 1;
            }
        }
        let rate = hits as f64 / trials as f64;
        let expected = VERIFICATION_SAMPLES_PER_TEMPO as f64 / RSV_EXPECTED_SUBS_PER_TEMPO as f64;
        let drift = (rate - expected).abs();
        assert!(
            drift < 0.005,
            "rate {rate:.4} drifts {drift:.4} from expected {expected:.4}"
        );
    }

    #[test]
    fn strikes_keyed_by_hotkey_not_uid() {
        let mut mgr = fresh();
        let hotkey = "5HotkeyValueX";
        let other_hotkey = "5OtherHotkey";

        for i in 0..(VERIFICATION_STRIKES_REQUIRED.saturating_sub(1)) {
            assert!(!mgr.record_strike(hotkey, 100 + i as u64, 360));
        }

        let triggered = mgr.record_strike(hotkey, 200, 360);
        assert!(
            triggered,
            "strike #{VERIFICATION_STRIKES_REQUIRED} must trigger skiplist"
        );
        assert!(mgr.is_skiplisted(hotkey, 200));
        assert!(!mgr.is_skiplisted(other_hotkey, 200));
    }

    #[test]
    fn inactive_hotkey_pruned_after_window() {
        let mut mgr = fresh();
        mgr.observe("hk_active", 1000);
        mgr.observe("hk_idle", 1000);
        mgr.strikes
            .entry("hk_idle".to_string())
            .or_default()
            .push_back(1000);

        let mid = 1000 + VERIFICATION_STRIKES_WINDOW_BLOCKS + 10;
        mgr.observe("hk_active", mid);
        mgr.prune_expired(mid, 360);

        assert!(mgr.coldstart.contains_key("hk_idle"));
        assert!(mgr.last_seen.contains_key("hk_idle"));
        assert!(!mgr.strikes.contains_key("hk_idle"));

        let far = 1000 + VERIFICATION_COLDSTART_RETENTION_BLOCKS + 10;
        mgr.observe("hk_active", far);
        mgr.prune_expired(far, 360);

        assert!(mgr.coldstart.contains_key("hk_active"));
        assert!(mgr.last_seen.contains_key("hk_active"));
        assert!(!mgr.coldstart.contains_key("hk_idle"));
        assert!(!mgr.last_seen.contains_key("hk_idle"));
    }

    #[test]
    fn history_cap_drops_oldest_when_exceeded() {
        let mut mgr = fresh();
        for i in 0..(VERIFICATION_HISTORY_CAP + 5) {
            mgr.observe(&format!("hk_{i}"), 1000 + i as u64);
        }
        mgr.prune_expired(2000 + VERIFICATION_HISTORY_CAP as u64, 360);
        assert!(mgr.last_seen.len() <= VERIFICATION_HISTORY_CAP);
        assert!(!mgr.last_seen.contains_key("hk_0"));
        assert!(mgr
            .last_seen
            .contains_key(&format!("hk_{}", VERIFICATION_HISTORY_CAP + 4)));
    }

    #[test]
    fn skiplist_uses_fallback_tempo_when_unknown() {
        let mut mgr = fresh();
        for _ in 0..VERIFICATION_STRIKES_REQUIRED {
            mgr.record_strike("hk", 1000, 0);
        }
        assert!(mgr.is_skiplisted("hk", 1000));
        assert!(mgr.is_skiplisted("hk", 1000 + 100));
    }
}
