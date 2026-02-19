use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{CircuitType, ProofSystem};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitPaths {
    pub model_id: String,
    pub base_path: PathBuf,
    pub input: PathBuf,
    pub metadata: PathBuf,
    pub compiled_model: PathBuf,
    pub pk: PathBuf,
    pub vk: PathBuf,
    pub settings: PathBuf,
    pub witness: PathBuf,
    pub proof: PathBuf,
    pub srs: PathBuf,
    pub witness_executable: PathBuf,
    pub full_model: PathBuf,
}

impl CircuitPaths {
    pub fn new(model_id: &str, cache_dir: &str) -> Self {
        let base = PathBuf::from(cache_dir).join(model_id);
        Self {
            model_id: model_id.to_string(),
            base_path: base.clone(),
            input: base.join("input.json"),
            metadata: base.join("circuit_metadata.json"),
            compiled_model: base.join("model.compiled"),
            pk: base.join("circuit.zkey"),
            vk: base.join("verification_key.json"),
            settings: base.join("settings.json"),
            witness: base.join("witness.json"),
            proof: base.join("proof.json"),
            srs: base.join("srs.bin"),
            witness_executable: base.join("witness.js"),
            full_model: base.join("full_model.onnx"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitMetadata {
    pub name: String,
    pub description: String,
    pub author: String,
    pub version: String,
    #[serde(rename = "type")]
    pub circuit_type: CircuitType,
    pub external_files: Option<HashMap<String, String>>,
    pub proof_system: String,
    pub dslices: Option<Vec<HashMap<String, serde_json::Value>>>,
    pub netuid: Option<i32>,
    pub weights_version: Option<i32>,
    pub timeout: Option<i32>,
    pub benchmark_choice_weight: Option<f64>,
    pub input_schema: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Circuit {
    pub id: String,
    pub paths: CircuitPaths,
    pub metadata: CircuitMetadata,
    pub proof_system: ProofSystem,
    pub settings: HashMap<String, serde_json::Value>,
    pub timeout: f64,
}

impl Circuit {
    pub fn validate_inputs(&self, inputs: &serde_json::Value) -> Result<(), String> {
        let schema = match &self.metadata.input_schema {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };

        let input_obj = match inputs.as_object() {
            Some(obj) => obj,
            None => return Err("inputs must be a JSON object".to_string()),
        };

        for key in schema.keys() {
            if !input_obj.contains_key(key) {
                return Err(format!("missing required input: {key}"));
            }
        }

        Ok(())
    }
}
