use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use anyhow::{bail, Result};
use tracing::{info, warn};

use crate::reconstruct;

const DEFAULT_MAX_TILES: usize = 65536;

pub struct StoredTile {
    pub data: Vec<f64>,
    pub channels: usize,
    pub height: usize,
    pub width: usize,
}

struct TileStoreInner {
    map: HashMap<String, StoredTile>,
    insertion_order: VecDeque<String>,
    max_capacity: usize,
}

pub struct TileStore {
    data: Mutex<TileStoreInner>,
}

impl Default for TileStore {
    fn default() -> Self {
        Self::new()
    }
}

fn lock_store(mutex: &Mutex<TileStoreInner>) -> Result<std::sync::MutexGuard<'_, TileStoreInner>> {
    mutex
        .lock()
        .map_err(|e| anyhow::anyhow!("tile store lock poisoned: {e}"))
}

impl TileStore {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_TILES)
    }

    pub fn with_capacity(max_capacity: usize) -> Self {
        Self {
            data: Mutex::new(TileStoreInner {
                map: HashMap::new(),
                insertion_order: VecDeque::new(),
                max_capacity,
            }),
        }
    }

    pub fn insert(&self, key: String, tile: StoredTile) -> Result<()> {
        let expected = tile
            .channels
            .checked_mul(tile.height)
            .and_then(|v| v.checked_mul(tile.width))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "tile shape overflow: {}x{}x{}",
                    tile.channels,
                    tile.height,
                    tile.width
                )
            })?;
        if tile.data.len() != expected {
            bail!(
                "tile data length {} != expected {} ({}x{}x{})",
                tile.data.len(),
                expected,
                tile.channels,
                tile.height,
                tile.width
            );
        }
        let mut inner = lock_store(&self.data)?;

        if inner.max_capacity == 0 {
            return Ok(());
        }

        #[allow(clippy::map_entry)]
        if inner.map.contains_key(&key) {
            inner.map.insert(key, tile);
        } else {
            while inner.map.len() >= inner.max_capacity {
                if let Some(oldest_key) = inner.insertion_order.pop_front() {
                    if inner.map.remove(&oldest_key).is_some() {
                        warn!(evicted = %oldest_key, capacity = inner.max_capacity, "tile evicted due to capacity limit");
                    }
                } else {
                    break;
                }
            }
            info!(key = %key, len = tile.data.len(), "tile stored");
            inner.insertion_order.push_back(key.clone());
            inner.map.insert(key, tile);
        }
        Ok(())
    }

    pub fn reconstruct(
        &self,
        tile_keys: &[String],
        tiles_y: usize,
        tiles_x: usize,
    ) -> Result<Vec<f64>> {
        let expected = tiles_y.checked_mul(tiles_x).ok_or_else(|| {
            anyhow::anyhow!("tiles_y * tiles_x overflow: tiles_y={tiles_y} tiles_x={tiles_x}")
        })?;
        if tile_keys.len() != expected {
            bail!(
                "tile_keys length {} != tiles_y({}) * tiles_x({})",
                tile_keys.len(),
                tiles_y,
                tiles_x
            );
        }
        if expected == 0 {
            return Ok(vec![]);
        }
        let unique: HashSet<&String> = tile_keys.iter().collect();
        if unique.len() != tile_keys.len() {
            bail!(
                "duplicate tile keys detected: {} unique out of {} (tiles_y={}, tiles_x={})",
                unique.len(),
                tile_keys.len(),
                tiles_y,
                tiles_x
            );
        }

        let inner = lock_store(&self.data)?;

        let first = inner
            .map
            .get(&tile_keys[0])
            .ok_or_else(|| anyhow::anyhow!("missing tile key: {}", tile_keys[0]))?;
        let channels = first.channels;
        let tile_h = first.height;
        let tile_w = first.width;

        let mut tile_refs: Vec<&[f64]> = Vec::with_capacity(expected);
        for key in tile_keys {
            let entry = inner
                .map
                .get(key)
                .ok_or_else(|| anyhow::anyhow!("missing tile key: {}", key))?;
            if entry.channels != channels || entry.height != tile_h || entry.width != tile_w {
                bail!(
                    "tile shape mismatch for {}: [{},{},{}] vs expected [{},{},{}]",
                    key,
                    entry.channels,
                    entry.height,
                    entry.width,
                    channels,
                    tile_h,
                    tile_w
                );
            }
            tile_refs.push(&entry.data);
        }

        reconstruct::grid_reconstruct(&tile_refs, tiles_y, tiles_x, channels, tile_h, tile_w)
    }

    pub fn evict(&self, keys: &[String]) -> Result<usize> {
        let mut inner = lock_store(&self.data)?;
        let mut removed = 0;
        for key in keys {
            if inner.map.remove(key).is_some() {
                removed += 1;
            }
        }
        if removed > 0 {
            let remove_set: HashSet<&str> = keys.iter().map(String::as_str).collect();
            inner
                .insertion_order
                .retain(|k| !remove_set.contains(k.as_str()));
            info!(removed, remaining = inner.map.len(), "tiles evicted");
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tile(data: Vec<f64>) -> StoredTile {
        StoredTile {
            data,
            channels: 1,
            height: 2,
            width: 2,
        }
    }

    #[test]
    fn insert_and_evict() {
        let store = TileStore::new();
        store.insert("a".into(), make_tile(vec![1.0; 4])).unwrap();
        store.insert("b".into(), make_tile(vec![2.0; 4])).unwrap();
        let removed = store.evict(&["a".into()]).unwrap();
        assert_eq!(removed, 1);
        let removed = store.evict(&["a".into()]).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn reconstruct_single_tile() {
        let store = TileStore::new();
        store
            .insert("t0".into(), make_tile(vec![1.0, 2.0, 3.0, 4.0]))
            .unwrap();
        let result = store.reconstruct(&["t0".into()], 1, 1).unwrap();
        assert_eq!(result, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn reconstruct_missing_tile_errors() {
        let store = TileStore::new();
        let result = store.reconstruct(&["missing".into()], 1, 1);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing tile key"));
    }

    #[test]
    fn reconstruct_length_mismatch_errors() {
        let store = TileStore::new();
        store.insert("a".into(), make_tile(vec![1.0; 4])).unwrap();
        let result = store.reconstruct(&["a".into()], 2, 2);
        assert!(result.is_err());
    }

    #[test]
    fn insert_rejects_malformed_length() {
        let store = TileStore::new();
        let tile = StoredTile {
            data: vec![1.0; 3],
            channels: 1,
            height: 2,
            width: 2,
        };
        let result = store.insert("bad".into(), tile);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("length"));
    }

    #[test]
    fn insert_rejects_overflow_shape() {
        let store = TileStore::new();
        let tile = StoredTile {
            data: vec![],
            channels: usize::MAX,
            height: usize::MAX,
            width: usize::MAX,
        };
        let result = store.insert("overflow".into(), tile);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("overflow"));
    }

    #[test]
    fn reconstruct_duplicate_keys_errors() {
        let store = TileStore::new();
        store.insert("a".into(), make_tile(vec![1.0; 4])).unwrap();
        store.insert("b".into(), make_tile(vec![2.0; 4])).unwrap();
        let result = store.reconstruct(&["a".into(), "a".into(), "b".into(), "b".into()], 2, 2);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("duplicate tile keys"));
    }

    #[test]
    fn capacity_evicts_oldest() {
        let store = TileStore::with_capacity(2);
        store.insert("a".into(), make_tile(vec![1.0; 4])).unwrap();
        store.insert("b".into(), make_tile(vec![2.0; 4])).unwrap();
        store.insert("c".into(), make_tile(vec![3.0; 4])).unwrap();
        let inner = lock_store(&store.data).unwrap();
        assert_eq!(inner.map.len(), 2);
        assert!(!inner.map.contains_key("a"));
        assert!(inner.map.contains_key("b"));
        assert!(inner.map.contains_key("c"));
    }

    #[test]
    fn overwrite_existing_key_does_not_evict() {
        let store = TileStore::with_capacity(2);
        store.insert("a".into(), make_tile(vec![1.0; 4])).unwrap();
        store.insert("b".into(), make_tile(vec![2.0; 4])).unwrap();
        store.insert("a".into(), make_tile(vec![9.0; 4])).unwrap();
        let inner = lock_store(&store.data).unwrap();
        assert_eq!(inner.map.len(), 2);
        assert!(inner.map.contains_key("a"));
        assert!(inner.map.contains_key("b"));
        assert_eq!(inner.map.get("a").unwrap().data[0], 9.0);
    }
}
