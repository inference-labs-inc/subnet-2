use anyhow::Result;
use serde_json::json;
use sn2_circuit_store::CircuitStore;
use tokio::sync::Mutex;
use tracing::info;

use sn2_types::*;

use crate::dsperse::DSperseClient;

pub struct MinerHandlers {
    dsperse: DSperseClient,
    circuit_store: Mutex<CircuitStore>,
}

impl MinerHandlers {
    pub fn new(dsperse: DSperseClient, circuit_store: CircuitStore) -> Self {
        Self {
            dsperse,
            circuit_store: Mutex::new(circuit_store),
        }
    }

    pub async fn handle_query_zk_proof(&self, data: QueryZkProof) -> Result<serde_json::Value> {
        let model_id = data.model_id.as_deref().unwrap_or("");
        info!(model_id = model_id, "handling QueryZkProof");

        if !model_id.is_empty() {
            self.circuit_store
                .lock()
                .await
                .ensure_circuit(model_id)
                .await?;
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

        self.circuit_store
            .lock()
            .await
            .ensure_circuit(&data.verification_key_hash)
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

        info!(circuit = circuit_id, slice = slice_num, "handling DSlice");

        if !circuit_id.is_empty() {
            self.circuit_store
                .lock()
                .await
                .ensure_circuit(circuit_id)
                .await?;
        }

        let result = self
            .dsperse
            .prove_slice(circuit_id, slice_num, &data.inputs.unwrap_or(json!({})))
            .await?;

        Ok(result)
    }
}
