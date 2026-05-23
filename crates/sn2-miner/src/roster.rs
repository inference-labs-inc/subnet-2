use std::collections::HashMap;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use parity_scale_codec::{Decode, Encode};
use tracing::{debug, warn};

const ROSTER_FILE_NAME: &str = "validator-roster.scale";
const CURRENT_VERSION: u8 = 1;

#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
pub enum IpRepr {
    V4([u8; 4]),
    V6([u8; 16]),
}

impl From<IpAddr> for IpRepr {
    fn from(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => IpRepr::V4(v4.octets()),
            IpAddr::V6(v6) => IpRepr::V6(v6.octets()),
        }
    }
}

impl From<IpRepr> for IpAddr {
    fn from(rep: IpRepr) -> Self {
        match rep {
            IpRepr::V4(b) => IpAddr::V4(Ipv4Addr::from(b)),
            IpRepr::V6(b) => IpAddr::V6(Ipv6Addr::from(b)),
        }
    }
}

#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
pub struct RosterEntry {
    pub hotkey: String,
    pub ip: IpRepr,
    pub last_seen_unix: u64,
    pub stake_snapshot: u64,
}

#[derive(Debug, Clone, Encode, Decode, PartialEq, Eq)]
pub struct RosterFile {
    pub version: u8,
    pub netuid: u16,
    pub entries: Vec<RosterEntry>,
}

/// In-memory roster: hotkey -> (ip, last_seen, stake_snapshot). The roster represents
/// the *most recent* observed source IP per validator hotkey; rotations overwrite the
/// previous entry rather than accumulating.
#[derive(Debug, Default, Clone)]
pub struct Roster {
    entries: HashMap<String, RosterEntry>,
}

impl Roster {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn ip_for(&self, hotkey: &str) -> Option<IpAddr> {
        self.entries.get(hotkey).map(|e| e.ip.clone().into())
    }

    pub fn entries(&self) -> impl Iterator<Item = &RosterEntry> {
        self.entries.values()
    }

    /// Inserts or replaces a roster entry. Returns true when this is a new hotkey or
    /// the IP changed for an existing one (caller can use this to gate writeback).
    pub fn upsert(
        &mut self,
        hotkey: &str,
        ip: IpAddr,
        last_seen_unix: u64,
        stake_snapshot: u64,
    ) -> bool {
        let ip_rep = IpRepr::from(ip);
        match self.entries.get_mut(hotkey) {
            Some(existing) => {
                if existing.ip == ip_rep {
                    existing.last_seen_unix = last_seen_unix;
                    existing.stake_snapshot = stake_snapshot;
                    false
                } else {
                    existing.ip = ip_rep;
                    existing.last_seen_unix = last_seen_unix;
                    existing.stake_snapshot = stake_snapshot;
                    true
                }
            }
            None => {
                self.entries.insert(
                    hotkey.to_string(),
                    RosterEntry {
                        hotkey: hotkey.to_string(),
                        ip: ip_rep,
                        last_seen_unix,
                        stake_snapshot,
                    },
                );
                true
            }
        }
    }

    pub fn to_file(&self, netuid: u16) -> RosterFile {
        let mut entries: Vec<RosterEntry> = self.entries.values().cloned().collect();
        entries.sort_by(|a, b| a.hotkey.cmp(&b.hotkey));
        RosterFile {
            version: CURRENT_VERSION,
            netuid,
            entries,
        }
    }

    pub fn from_file(file: RosterFile) -> Self {
        let entries = file
            .entries
            .into_iter()
            .map(|e| (e.hotkey.clone(), e))
            .collect();
        Self { entries }
    }
}

/// Returns the on-disk path for a netuid-scoped roster file under `wallet_path`.
pub fn roster_path(wallet_path: &Path, netuid: u16) -> PathBuf {
    wallet_path
        .join("sn2-miner")
        .join(netuid.to_string())
        .join(ROSTER_FILE_NAME)
}

/// Loads a SCALE-encoded roster from disk. Returns `Ok(None)` if the file does not
/// exist; corrupted or version-mismatched files are logged at warn and treated as
/// absent so they get rewritten cleanly on first flush.
pub fn load(wallet_path: &Path, netuid: u16) -> Result<Option<Roster>> {
    let path = roster_path(wallet_path, netuid);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let decoded = match RosterFile::decode(&mut bytes.as_slice()) {
        Ok(v) => v,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "ignoring unreadable validator roster file");
            return Ok(None);
        }
    };
    if decoded.version != CURRENT_VERSION {
        warn!(
            path = %path.display(),
            found = decoded.version,
            expected = CURRENT_VERSION,
            "ignoring roster file with unexpected version",
        );
        return Ok(None);
    }
    if decoded.netuid != netuid {
        warn!(
            path = %path.display(),
            found = decoded.netuid,
            expected = netuid,
            "ignoring roster file for a different netuid",
        );
        return Ok(None);
    }
    debug!(path = %path.display(), entries = decoded.entries.len(), "loaded validator roster");
    Ok(Some(Roster::from_file(decoded)))
}

/// Persists a roster to disk atomically (write tmp, fsync, rename).
pub fn save(wallet_path: &Path, netuid: u16, roster: &Roster) -> Result<()> {
    let path = roster_path(wallet_path, netuid);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("scale.tmp");
    let file_repr = roster.to_file(netuid);
    let encoded = file_repr.encode();
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .with_context(|| format!("opening {} for write", tmp.display()))?;
        f.write_all(&encoded)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_returns_true_for_new_hotkey() {
        let mut r = Roster::new();
        assert!(r.upsert("hk1", "10.0.0.1".parse().unwrap(), 100, 500));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn upsert_returns_false_when_ip_unchanged() {
        let mut r = Roster::new();
        r.upsert("hk1", "10.0.0.1".parse().unwrap(), 100, 500);
        assert!(!r.upsert("hk1", "10.0.0.1".parse().unwrap(), 200, 600));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn upsert_returns_true_on_ip_rotation() {
        let mut r = Roster::new();
        r.upsert("hk1", "10.0.0.1".parse().unwrap(), 100, 500);
        assert!(r.upsert("hk1", "10.0.0.2".parse().unwrap(), 200, 500));
        assert_eq!(r.len(), 1);
        assert_eq!(
            r.ip_for("hk1").unwrap(),
            "10.0.0.2".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn scale_roundtrip_preserves_entries() {
        let mut r = Roster::new();
        r.upsert("hk_a", "10.0.0.1".parse().unwrap(), 100, 500);
        r.upsert("hk_b", "2001:db8::1".parse().unwrap(), 200, 600);
        let encoded = r.to_file(2).encode();
        let decoded = RosterFile::decode(&mut encoded.as_slice()).unwrap();
        let r2 = Roster::from_file(decoded);
        assert_eq!(r2.len(), 2);
        assert_eq!(
            r2.ip_for("hk_a").unwrap(),
            "10.0.0.1".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            r2.ip_for("hk_b").unwrap(),
            "2001:db8::1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = Roster::new();
        r.upsert("hk_a", "203.0.113.7".parse().unwrap(), 42, 1000);
        save(dir.path(), 2, &r).unwrap();
        let loaded = load(dir.path(), 2).unwrap().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded.ip_for("hk_a").unwrap(),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn load_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path(), 2).unwrap().is_none());
    }

    #[test]
    fn load_returns_none_on_netuid_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = Roster::new();
        r.upsert("hk_a", "203.0.113.7".parse().unwrap(), 42, 1000);
        save(dir.path(), 2, &r).unwrap();
        // Stored under netuid=2's path, so loading netuid=3 should not find the file
        // at all. The netuid-mismatch guard inside the file body protects against
        // operators manually copying files between paths.
        assert!(load(dir.path(), 3).unwrap().is_none());
    }
}
