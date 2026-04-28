use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use rand::Rng;
use sn2_types::{
    RSV_EXPECTED_SUBS_PER_TEMPO, VERIFICATION_COLDSTART_BLOCKS, VERIFICATION_SAMPLES_PER_TEMPO,
    VERIFICATION_SKIPLIST_TEMPOS, VERIFICATION_STRIKES_REQUIRED,
    VERIFICATION_STRIKES_WINDOW_BLOCKS,
};
use tracing::{info, warn};

pub struct RsvManager {
    skiplist: HashMap<u16, u64>,
    strikes: HashMap<u16, VecDeque<u64>>,
    coldstart: HashMap<u16, u64>,
    sample_budget: HashMap<(u16, u64), u32>,
    persistence_path: Option<PathBuf>,
}

impl RsvManager {
    pub fn new_with_persistence(path: PathBuf) -> Self {
        let mut mgr = Self {
            skiplist: HashMap::new(),
            strikes: HashMap::new(),
            coldstart: HashMap::new(),
            sample_budget: HashMap::new(),
            persistence_path: Some(path),
        };
        mgr.load();
        mgr
    }

    pub fn is_skiplisted(&self, uid: u16, current_block: u64) -> bool {
        self.skiplist
            .get(&uid)
            .is_some_and(|&until| current_block < until)
    }

    pub fn is_in_coldstart(&self, uid: u16, current_block: u64) -> bool {
        match self.coldstart.get(&uid) {
            Some(&first_seen) => {
                current_block.saturating_sub(first_seen) < VERIFICATION_COLDSTART_BLOCKS
            }
            None => true,
        }
    }

    pub fn observe(&mut self, uid: u16, current_block: u64) {
        self.coldstart.entry(uid).or_insert(current_block);
    }

    pub fn should_sample(&mut self, uid: u16, current_block: u64, blocks_per_tempo: u64) -> bool {
        let tempo_idx = current_block.checked_div(blocks_per_tempo).unwrap_or(0);
        let key = (uid, tempo_idx);
        let budget = self
            .sample_budget
            .entry(key)
            .or_insert(VERIFICATION_SAMPLES_PER_TEMPO as u32);
        if *budget == 0 {
            return false;
        }
        let mut rng = rand::rng();
        let roll: u64 = rng.random_range(0..RSV_EXPECTED_SUBS_PER_TEMPO);
        if roll < VERIFICATION_SAMPLES_PER_TEMPO {
            *budget -= 1;
            true
        } else {
            false
        }
    }

    pub fn record_strike(&mut self, uid: u16, current_block: u64, blocks_per_tempo: u64) -> bool {
        let entry = self.strikes.entry(uid).or_default();
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
            let until = current_block + VERIFICATION_SKIPLIST_TEMPOS * blocks_per_tempo;
            self.skiplist.insert(uid, until);
            self.strikes.remove(&uid);
            warn!(
                uid,
                until_block = until,
                "rsv: strike threshold reached, miner skiplisted"
            );
            true
        } else {
            false
        }
    }

    pub fn sync_uids(&mut self, active_uids: &[u16]) {
        let active: std::collections::HashSet<u16> = active_uids.iter().copied().collect();
        self.skiplist.retain(|uid, _| active.contains(uid));
        self.strikes.retain(|uid, _| active.contains(uid));
        self.coldstart.retain(|uid, _| active.contains(uid));
        self.sample_budget
            .retain(|(uid, _), _| active.contains(uid));
    }

    pub fn prune_expired(&mut self, current_block: u64) {
        self.skiplist.retain(|_, until| *until > current_block);
    }

    pub fn save(&self) {
        let path = match &self.persistence_path {
            Some(p) => p,
            None => return,
        };
        let skiplist_json: serde_json::Map<String, serde_json::Value> = self
            .skiplist
            .iter()
            .map(|(uid, until)| (uid.to_string(), serde_json::json!(*until)))
            .collect();
        let strikes_json: serde_json::Map<String, serde_json::Value> = self
            .strikes
            .iter()
            .map(|(uid, deque)| {
                (
                    uid.to_string(),
                    serde_json::Value::Array(deque.iter().map(|b| serde_json::json!(*b)).collect()),
                )
            })
            .collect();
        let coldstart_json: serde_json::Map<String, serde_json::Value> = self
            .coldstart
            .iter()
            .map(|(uid, first)| (uid.to_string(), serde_json::json!(*first)))
            .collect();
        let data = serde_json::json!({
            "skiplist": skiplist_json,
            "strikes": strikes_json,
            "coldstart": coldstart_json,
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
            Err(_) => return,
        };
        let parsed: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "rsv load: parse failed, starting fresh");
                return;
            }
        };
        if let Some(map) = parsed.get("skiplist").and_then(|v| v.as_object()) {
            for (k, v) in map {
                if let (Ok(uid), Some(until)) = (k.parse::<u16>(), v.as_u64()) {
                    self.skiplist.insert(uid, until);
                }
            }
        }
        if let Some(map) = parsed.get("strikes").and_then(|v| v.as_object()) {
            for (k, v) in map {
                let uid: u16 = match k.parse() {
                    Ok(u) => u,
                    Err(_) => continue,
                };
                let arr = match v.as_array() {
                    Some(a) => a,
                    None => continue,
                };
                let deque: VecDeque<u64> = arr.iter().filter_map(|x| x.as_u64()).collect();
                if !deque.is_empty() {
                    self.strikes.insert(uid, deque);
                }
            }
        }
        if let Some(map) = parsed.get("coldstart").and_then(|v| v.as_object()) {
            for (k, v) in map {
                if let (Ok(uid), Some(first)) = (k.parse::<u16>(), v.as_u64()) {
                    self.coldstart.insert(uid, first);
                }
            }
        }
        info!(
            skiplisted = self.skiplist.len(),
            tracked_strikes = self.strikes.len(),
            observed = self.coldstart.len(),
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
            sample_budget: HashMap::new(),
            persistence_path: None,
        }
    }

    #[test]
    fn coldstart_gates_new_uids() {
        let mut mgr = fresh();
        mgr.observe(1, 1000);
        assert!(mgr.is_in_coldstart(1, 2000));
        assert!(!mgr.is_in_coldstart(1, 3000));
    }

    #[test]
    fn coldstart_unknown_uid_is_in_coldstart() {
        let mgr = fresh();
        assert!(mgr.is_in_coldstart(99, 1_000_000));
    }

    #[test]
    fn observe_does_not_overwrite_first_seen() {
        let mut mgr = fresh();
        mgr.observe(1, 1000);
        mgr.observe(1, 2000);
        assert_eq!(mgr.coldstart.get(&1).copied(), Some(1000));
    }

    #[test]
    fn record_strike_below_threshold_no_skiplist() {
        let mut mgr = fresh();
        let triggered = mgr.record_strike(1, 100, 360);
        assert!(!triggered);
        assert!(!mgr.is_skiplisted(1, 100));
    }

    #[test]
    fn record_strike_at_threshold_skiplists() {
        let mut mgr = fresh();
        for i in 0..VERIFICATION_STRIKES_REQUIRED {
            let triggered = mgr.record_strike(1, 100 + i as u64, 360);
            if i + 1 < VERIFICATION_STRIKES_REQUIRED {
                assert!(!triggered);
            } else {
                assert!(triggered);
            }
        }
        let block = 100 + (VERIFICATION_STRIKES_REQUIRED as u64) - 1;
        assert!(mgr.is_skiplisted(1, block));
        assert!(mgr.strikes.get(&1).is_none());
        let until = mgr.skiplist.get(&1).copied().unwrap();
        assert_eq!(until, block + VERIFICATION_SKIPLIST_TEMPOS * 360);
    }

    #[test]
    fn strike_aging_removes_old_strikes() {
        let mut mgr = fresh();
        mgr.record_strike(1, 100, 360);
        mgr.record_strike(1, 200, 360);
        let later = 200 + VERIFICATION_STRIKES_WINDOW_BLOCKS + 10;
        let triggered = mgr.record_strike(1, later, 360);
        assert!(!triggered);
        let strikes = mgr.strikes.get(&1).unwrap();
        assert_eq!(strikes.len(), 1);
        assert_eq!(strikes.front().copied(), Some(later));
    }

    #[test]
    fn sync_uids_drops_deregistered() {
        let mut mgr = fresh();
        mgr.observe(1, 100);
        mgr.observe(2, 100);
        mgr.skiplist.insert(2, 5000);
        mgr.strikes.entry(2).or_default().push_back(50);
        mgr.sample_budget.insert((2, 0), 5);
        mgr.sync_uids(&[1]);
        assert!(mgr.coldstart.contains_key(&1));
        assert!(!mgr.coldstart.contains_key(&2));
        assert!(!mgr.skiplist.contains_key(&2));
        assert!(!mgr.strikes.contains_key(&2));
        assert!(!mgr.sample_budget.contains_key(&(2, 0)));
    }

    #[test]
    fn prune_expired_drops_past_skiplist() {
        let mut mgr = fresh();
        mgr.skiplist.insert(1, 200);
        mgr.skiplist.insert(2, 5000);
        mgr.prune_expired(300);
        assert!(!mgr.skiplist.contains_key(&1));
        assert!(mgr.skiplist.contains_key(&2));
    }

    #[test]
    fn save_load_round_trip() {
        let path = temp_path("roundtrip");
        let mut mgr = RsvManager::new_with_persistence(path.clone());
        mgr.observe(7, 500);
        mgr.skiplist.insert(8, 9000);
        mgr.strikes.entry(9).or_default().push_back(123);
        mgr.save();

        let loaded = RsvManager::new_with_persistence(path.clone());
        assert_eq!(loaded.coldstart.get(&7).copied(), Some(500));
        assert_eq!(loaded.skiplist.get(&8).copied(), Some(9000));
        assert_eq!(loaded.strikes.get(&9).unwrap().front().copied(), Some(123));

        if let Some(parent) = path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }

    #[test]
    fn should_sample_respects_budget() {
        let mut mgr = fresh();
        mgr.sample_budget.insert((1, 0), 0);
        assert!(!mgr.should_sample(1, 50, 360));
    }
}
