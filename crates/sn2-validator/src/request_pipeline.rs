use std::collections::HashSet;

use sha2::{Digest, Sha256};
use sn2_types::{Circuit, DSliceProofGenerationDataModel, ProofSystem, Request, RequestType};

pub struct RequestPipeline {
    hash_guard: HashSet<String>,
}

impl RequestPipeline {
    pub fn new() -> Self {
        Self {
            hash_guard: HashSet::new(),
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

    pub fn prepare_benchmark_request(
        &mut self,
        _uid: u16,
        circuit: &Circuit,
        inputs: serde_json::Value,
    ) -> Option<Request> {
        let hash = compute_input_hash(&inputs);
        if self.hash_guard.contains(&hash) {
            return None;
        }
        self.hash_guard.insert(hash);

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
    }

    pub fn clear_guard(&mut self) {
        self.hash_guard.clear();
    }
}

fn compute_input_hash(inputs: &serde_json::Value) -> String {
    let bytes = serde_json::to_vec(inputs).unwrap_or_default();
    let hash = Sha256::digest(&bytes);
    hex::encode(hash)
}
