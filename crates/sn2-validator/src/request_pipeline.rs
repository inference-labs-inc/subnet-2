use std::collections::{HashSet, VecDeque};

use sha2::{Digest, Sha256};
use sn2_types::{Circuit, DSliceProofGenerationDataModel, ProofSystem, Request, RequestType};
use tracing::warn;

const MAX_HASHES: usize = 32768;

pub struct RequestPipeline {
    hash_guard: HashSet<String>,
    hash_order: VecDeque<String>,
    capacity: usize,
}

impl Default for RequestPipeline {
    fn default() -> Self {
        Self {
            hash_guard: HashSet::new(),
            hash_order: VecDeque::new(),
            capacity: MAX_HASHES,
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
            hash_guard: HashSet::new(),
            hash_order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    fn insert_hash(&mut self, hash: String) {
        while self.hash_guard.len() >= self.capacity {
            if let Some(oldest) = self.hash_order.pop_front() {
                self.hash_guard.remove(&oldest);
            } else {
                break;
            }
        }
        self.hash_guard.insert(hash.clone());
        self.hash_order.push_back(hash);
    }

    pub fn check_hash(&mut self, inputs: &serde_json::Value) -> Option<String> {
        let hash = compute_input_hash(inputs);
        if self.hash_guard.contains(&hash) {
            return None;
        }
        self.insert_hash(hash.clone());
        Some(hash)
    }

    pub fn check_dslice_hash(
        &mut self,
        circuit_id: &str,
        slice_num: &str,
        run_uid: &str,
    ) -> Option<String> {
        let mut hasher = Sha256::new();
        hasher.update(circuit_id.as_bytes());
        hasher.update(b":");
        hasher.update(slice_num.as_bytes());
        hasher.update(b":");
        hasher.update(run_uid.as_bytes());
        let hash = hex::encode(hasher.finalize());
        if self.hash_guard.contains(&hash) {
            return None;
        }
        self.insert_hash(hash.clone());
        Some(hash)
    }

    fn check_benchmark_hash(
        &mut self,
        circuit_id: &str,
        inputs: &serde_json::Value,
    ) -> Option<String> {
        let input_bytes = serialize_or_null(inputs);
        let mut hasher = Sha256::new();
        hasher.update(circuit_id.as_bytes());
        hasher.update(b":");
        hasher.update(&input_bytes);
        let hash = hex::encode(hasher.finalize());
        if self.hash_guard.contains(&hash) {
            return None;
        }
        self.insert_hash(hash.clone());
        Some(hash)
    }

    pub fn prepare_benchmark_request(
        &mut self,
        circuit: &Circuit,
        inputs: serde_json::Value,
    ) -> Option<Request> {
        self.check_benchmark_hash(&circuit.id, &inputs)?;
        Some(Request {
            circuit: circuit.clone(),
            inputs,
            request_type: RequestType::Benchmark,
            retry_count: 0,
        })
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
    ) -> DSliceProofGenerationDataModel {
        DSliceProofGenerationDataModel {
            circuit: Some(circuit.id.clone()),
            proof_system,
            inputs: Some(inputs),
            outputs,
            slice_num: Some(slice_num.to_string()),
            run_uid: Some(run_uid.to_string()),
        }
    }

    pub fn release_hash(&mut self, hash: &str) {
        self.hash_guard.remove(hash);
        if let Some(pos) = self.hash_order.iter().position(|h| h == hash) {
            self.hash_order.remove(pos);
        }
    }

    pub fn clear_guard(&mut self) {
        self.hash_guard.clear();
        self.hash_order.clear();
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
            .check_dslice_hash("circuit1", "slice0", "run1")
            .is_some());
        assert!(pipeline
            .check_dslice_hash("circuit1", "slice0", "run1")
            .is_none());
    }

    #[test]
    fn check_dslice_hash_varies_with_component() {
        let mut pipeline = RequestPipeline::new();
        assert!(pipeline
            .check_dslice_hash("circuit1", "slice0", "run1")
            .is_some());
        assert!(pipeline
            .check_dslice_hash("circuit1", "slice0", "run2")
            .is_some());
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

    fn make_test_circuit(id: &str) -> Circuit {
        use std::collections::HashMap;
        Circuit {
            id: id.into(),
            paths: sn2_types::CircuitPaths::new(id, "/tmp"),
            metadata: sn2_types::CircuitMetadata {
                name: String::new(),
                description: String::new(),
                author: String::new(),
                version: String::new(),
                circuit_type: sn2_types::CircuitType::PROOF_OF_COMPUTATION,
                external_files: None,
                proof_system: "groth16".into(),
                dslices: None,
                netuid: None,
                weights_version: None,
                timeout: None,
                benchmark_choice_weight: None,
                input_schema: None,
            },
            proof_system: ProofSystem::ZKML,
            settings: HashMap::new(),
            timeout: 60.0,
        }
    }

    #[test]
    fn prepare_benchmark_request_deduplicates() {
        let mut pipeline = RequestPipeline::new();
        let circuit = make_test_circuit("test");
        let inputs = serde_json::json!({"x": 1});
        assert!(pipeline
            .prepare_benchmark_request(&circuit, inputs.clone())
            .is_some());
        assert!(pipeline
            .prepare_benchmark_request(&circuit, inputs)
            .is_none());
    }

    #[test]
    fn prepare_benchmark_different_circuits_same_inputs() {
        let mut pipeline = RequestPipeline::new();
        let inputs = serde_json::json!({"x": 1});
        assert!(pipeline
            .prepare_benchmark_request(&make_test_circuit("circuit_a"), inputs.clone())
            .is_some());
        assert!(
            pipeline
                .prepare_benchmark_request(&make_test_circuit("circuit_b"), inputs)
                .is_some(),
            "different circuit with same inputs should not collide"
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
