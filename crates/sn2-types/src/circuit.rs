use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{CircuitType, ProofSystem};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitPaths {
    pub model_id: String,
    pub base_path: PathBuf,
}

impl CircuitPaths {
    pub fn new(model_id: &str, cache_dir: &str) -> Self {
        let base = PathBuf::from(cache_dir).join(model_id);
        Self {
            model_id: model_id.to_string(),
            base_path: base,
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
