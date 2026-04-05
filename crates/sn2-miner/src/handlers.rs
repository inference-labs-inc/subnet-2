use anyhow::Result;
use serde_json::json;
use sn2_circuit_store::CircuitStore;
use tokio::sync::RwLock;
use tracing::info;

use sn2_types::*;

use crate::dsperse::{normalize_slice_id, DSperseClient};

pub struct MinerHandlers {
    dsperse: DSperseClient,
    circuit_store: RwLock<CircuitStore>,
}

impl MinerHandlers {
    pub fn new(dsperse: DSperseClient, circuit_store: CircuitStore) -> Self {
        Self {
            dsperse,
            circuit_store: RwLock::new(circuit_store),
        }
    }

    async fn ensure_circuit_cached(&self, circuit_id: &str) -> Result<()> {
        {
            let store = self.circuit_store.read().await;
            if store.get_circuit(circuit_id).is_some() {
                return Ok(());
            }
        }
        let mut store = self.circuit_store.write().await;
        if store.get_circuit(circuit_id).is_some() {
            return Ok(());
        }
        store.ensure_circuit(circuit_id).await?;
        Ok(())
    }

    pub async fn handle_query_zk_proof(&self, data: QueryZkProof) -> Result<serde_json::Value> {
        let model_id = data.model_id.as_deref().unwrap_or("");
        info!(model_id = model_id, "handling QueryZkProof");

        if !model_id.is_empty() {
            self.ensure_circuit_cached(model_id).await?;
        }

        let result = self
            .dsperse
            .prove(model_id, &data.query_input.unwrap_or(json!({})))
            .await?;

        Ok(json!({
            "query_output": result.get("proof").and_then(|v| v.as_str()).unwrap_or(""),
            "witness": result.get("witness").and_then(|v| v.as_str()).unwrap_or(""),
            "computed_outputs": result.get("computed_outputs").cloned().unwrap_or(json!([])),
        }))
    }

    pub async fn handle_proof_of_weights(
        &self,
        data: ProofOfWeightsDataModel,
    ) -> Result<serde_json::Value> {
        info!(
            vk_hash = %data.verification_key_hash,
            "handling ProofOfWeights"
        );

        self.ensure_circuit_cached(&data.verification_key_hash)
            .await?;

        let result = self
            .dsperse
            .prove(&data.verification_key_hash, &data.inputs)
            .await?;

        Ok(json!({
            "proof": result.get("proof").and_then(|v| v.as_str()).unwrap_or(""),
            "public_signals": result.get("public_signals").and_then(|v| v.as_str()).unwrap_or(""),
        }))
    }

    pub async fn handle_dslice(
        &self,
        data: DSliceProofGenerationDataModel,
    ) -> Result<serde_json::Value> {
        let circuit_id = data.circuit.as_deref().unwrap_or("");
        let slice_num = data.slice_num.as_deref().unwrap_or("");
        let component_sha = data.component_sha.as_deref();

        info!(
            circuit = circuit_id,
            slice = slice_num,
            component_sha = component_sha.unwrap_or("none"),
            "handling DSlice"
        );

        let resolved_dir = match component_sha {
            Some(sha) => {
                let slice_id = normalize_slice_id(slice_num)?;
                self.dsperse.resolve_component(sha, &slice_id).await?
            }
            None => None,
        };

        if resolved_dir.is_none() && !circuit_id.is_empty() {
            self.ensure_circuit_cached(circuit_id).await?;
        }

        let inputs = data.inputs.unwrap_or(json!({}));

        let dir = match resolved_dir {
            Some(dir) => dir,
            None => {
                let slice_id = normalize_slice_id(slice_num)?;
                let slices_dir = std::path::PathBuf::from(
                    shellexpand::tilde(sn2_types::CIRCUIT_CACHE_DIR).to_string(),
                )
                .join(format!("model_{circuit_id}"))
                .join("slices");
                slices_dir.join(slice_id)
            }
        };

        let result = self
            .dsperse
            .prove_slice(circuit_id, slice_num, &inputs, dir)
            .await?;

        Ok(result)
    }
}
