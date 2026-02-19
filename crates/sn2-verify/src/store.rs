use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{bail, Result};
use tracing::info;

use crate::reconstruct;

pub struct StoredTile {
    pub data: Vec<f64>,
    pub channels: usize,
    pub height: usize,
    pub width: usize,
}

pub struct TileStore {
    data: Mutex<HashMap<String, StoredTile>>,
}

impl Default for TileStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TileStore {
    pub fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
        }
    }

    pub fn insert(&self, key: String, tile: StoredTile) {
        let mut map = self.data.lock().unwrap();
        info!(key = %key, len = tile.data.len(), "tile stored");
        map.insert(key, tile);
    }

    pub fn reconstruct(
        &self,
        tile_keys: &[String],
        tiles_y: usize,
        tiles_x: usize,
    ) -> Result<Vec<f64>> {
        let expected = tiles_y * tiles_x;
        if expected == 0 {
            return Ok(vec![]);
        }
        if tile_keys.len() != expected {
            bail!(
                "tile_keys length {} != tiles_y({}) * tiles_x({})",
                tile_keys.len(),
                tiles_y,
                tiles_x
            );
        }

        let map = self.data.lock().unwrap();

        let first = map
            .get(&tile_keys[0])
            .ok_or_else(|| anyhow::anyhow!("missing tile key: {}", tile_keys[0]))?;
        let channels = first.channels;
        let tile_h = first.height;
        let tile_w = first.width;

        let mut tile_refs: Vec<&[f64]> = Vec::with_capacity(expected);
        for key in tile_keys {
            let entry = map
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

        Ok(reconstruct::grid_reconstruct(
            &tile_refs, tiles_y, tiles_x, channels, tile_h, tile_w,
        ))
    }

    pub fn evict(&self, keys: &[String]) -> usize {
        let mut map = self.data.lock().unwrap();
        let mut removed = 0;
        for key in keys {
            if map.remove(key).is_some() {
                removed += 1;
            }
        }
        if removed > 0 {
            info!(removed, remaining = map.len(), "tiles evicted");
        }
        removed
    }
}
