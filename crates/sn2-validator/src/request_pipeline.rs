use sha2::{Digest, Sha256};
use sn2_types::{BoundedFifoSet, Circuit, DSliceProofGenerationDataModel, ProofSystem};
use tracing::warn;

const MAX_HASHES: usize = 32768;

pub struct RequestPipeline {
    hash_guard: BoundedFifoSet<String>,
}

impl Default for RequestPipeline {
    fn default() -> Self {
        Self {
            hash_guard: BoundedFifoSet::new(MAX_HASHES),
        }
    }
}

impl RequestPipeline {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_capacity(capacity: usize) -> Self {
        Self {
            hash_guard: BoundedFifoSet::new(capacity),
        }
    }

    pub fn check_hash(&mut self, inputs: &serde_json::Value) -> Option<String> {
        let hash = compute_input_hash(inputs);
        if self.hash_guard.contains(&hash) {
            return None;
        }
        self.hash_guard.insert(hash.clone());
        Some(hash)
    }

    pub fn check_dslice_hash(
        &mut self,
        circuit_id: &str,
        slice_num: &str,
        run_uid: &str,
        tile_idx: Option<u32>,
    ) -> Option<String> {
        let mut hasher = Sha256::new();
        hasher.update(circuit_id.as_bytes());
        hasher.update(b":");
        hasher.update(slice_num.as_bytes());
        hasher.update(b":");
        hasher.update(run_uid.as_bytes());
        if let Some(idx) = tile_idx {
            hasher.update(b":");
            hasher.update(idx.to_string().as_bytes());
        }
        let hash = hex::encode(hasher.finalize());
        if self.hash_guard.contains(&hash) {
            return None;
        }
        self.hash_guard.insert(hash.clone());
        Some(hash)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prepare_dslice_request(
        &mut self,
        _uid: u16,
        circuit: &Circuit,
        inputs: serde_json::Value,
        outputs: Option<serde_json::Value>,
        slice_num: &str,
        run_uid: &str,
        proof_system: ProofSystem,
        component_sha: Option<String>,
    ) -> DSliceProofGenerationDataModel {
        DSliceProofGenerationDataModel {
            circuit: Some(circuit.id.clone()),
            proof_system,
            inputs: Some(inputs),
            outputs,
            slice_num: Some(slice_num.to_string()),
            run_uid: Some(run_uid.to_string()),
            component_sha,
        }
    }

    pub fn release_hash(&mut self, hash: &str) {
        self.hash_guard.remove(hash);
    }

    pub fn clear_guard(&mut self) {
        self.hash_guard.clear();
    }
}

fn serialize_or_null(value: &serde_json::Value) -> Vec<u8> {
    match serde_json::to_vec(value) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "failed to serialize value for hashing, using null sentinel");
            serde_json::to_vec(&serde_json::Value::Null).unwrap_or_default()
        }
    }
}

fn compute_input_hash(inputs: &serde_json::Value) -> String {
    let bytes = serialize_or_null(inputs);
    let hash = Sha256::digest(&bytes);
    hex::encode(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_hash_returns_some_on_first_insert() {
        let mut pipeline = RequestPipeline::new();
        let inputs = serde_json::json!({"a": 1});
        assert!(pipeline.check_hash(&inputs).is_some());
    }

    #[test]
    fn check_hash_returns_none_on_duplicate() {
        let mut pipeline = RequestPipeline::new();
        let inputs = serde_json::json!({"a": 1});
        pipeline.check_hash(&inputs);
        assert!(pipeline.check_hash(&inputs).is_none());
    }

    #[test]
    fn release_hash_allows_reinsert() {
        let mut pipeline = RequestPipeline::new();
        let inputs = serde_json::json!({"x": 42});
        let hash = pipeline.check_hash(&inputs).unwrap();
        pipeline.release_hash(&hash);
        assert!(pipeline.check_hash(&inputs).is_some());
    }

    #[test]
    fn check_dslice_hash_deduplicates() {
        let mut pipeline = RequestPipeline::new();
        assert!(pipeline
            .check_dslice_hash("circuit1", "slice0", "run1", None)
            .is_some());
        assert!(pipeline
            .check_dslice_hash("circuit1", "slice0", "run1", None)
            .is_none());
    }

    #[test]
    fn check_dslice_hash_varies_with_component() {
        let mut pipeline = RequestPipeline::new();
        assert!(pipeline
            .check_dslice_hash("circuit1", "slice0", "run1", None)
            .is_some());
        assert!(pipeline
            .check_dslice_hash("circuit1", "slice0", "run2", None)
            .is_some());
    }

    #[test]
    fn check_dslice_hash_varies_with_tile_idx() {
        let mut pipeline = RequestPipeline::new();
        assert!(pipeline
            .check_dslice_hash("circuit1", "slice0", "run1", Some(0))
            .is_some());
        assert!(pipeline
            .check_dslice_hash("circuit1", "slice0", "run1", Some(1))
            .is_some());
        assert!(pipeline
            .check_dslice_hash("circuit1", "slice0", "run1", Some(0))
            .is_none());
    }

    #[test]
    fn different_inputs_produce_different_hashes() {
        let mut pipeline = RequestPipeline::new();
        let h1 = pipeline.check_hash(&serde_json::json!({"a": 1})).unwrap();
        let h2 = pipeline.check_hash(&serde_json::json!({"a": 2})).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn eviction_at_capacity() {
        let mut pipeline = RequestPipeline::with_capacity(8);
        let first_input = serde_json::json!({"i": -1});
        pipeline.check_hash(&first_input);
        for i in 0..8 {
            pipeline.check_hash(&serde_json::json!({"i": i}));
        }
        assert!(
            pipeline.check_hash(&first_input).is_some(),
            "earliest hash should have been evicted"
        );
    }

    #[test]
    fn clear_guard_resets_state() {
        let mut pipeline = RequestPipeline::new();
        let inputs = serde_json::json!({"key": "val"});
        pipeline.check_hash(&inputs);
        pipeline.clear_guard();
        assert!(pipeline.check_hash(&inputs).is_some());
    }
}
