use anyhow::Result;
use serde_json::json;
use tracing::info;

use sn2_types::*;

use crate::circuit_manager::CircuitManager;
use crate::dsperse::DSperseClient;

pub struct MinerHandlers {
    dsperse: DSperseClient,
    circuit_manager: std::sync::Arc<CircuitManager>,
}

impl MinerHandlers {
    pub fn new(dsperse: DSperseClient, circuit_manager: std::sync::Arc<CircuitManager>) -> Self {
        Self {
            dsperse,
            circuit_manager,
        }
    }

    pub async fn handle_query_zk_proof(&self, data: QueryZkProof) -> Result<serde_json::Value> {
        let model_id = data.model_id.as_deref().unwrap_or("");
        info!(model_id = model_id, "handling QueryZkProof");

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

        let result = self
            .dsperse
            .prove(&data.verification_key_hash, &data.inputs)
            .await?;

        Ok(json!({
            "proof": result.get("proof").and_then(|v| v.as_str()).unwrap_or(""),
            "public_signals": result.get("public_signals").and_then(|v| v.as_str()).unwrap_or(""),
        }))
    }

    pub async fn handle_competition(&self, data: Competition) -> Result<serde_json::Value> {
        info!(id = data.id, hash = %data.hash, file = %data.file_name, "handling Competition");

        let commitment = self.circuit_manager.get_commitment().await?;

        match commitment {
            Some(c) => Ok(json!({
                "id": data.id,
                "hash": data.hash,
                "file_name": data.file_name,
                "commitment": c.get("signature"),
                "file_content": c.get("file_urls").and_then(|u| u.get(&data.file_name)),
            })),
            None => Ok(json!({
                "id": data.id,
                "hash": data.hash,
                "file_name": data.file_name,
                "error": "no commitment available",
            })),
        }
    }

    pub async fn handle_dslice(
        &self,
        data: DSliceProofGenerationDataModel,
    ) -> Result<serde_json::Value> {
        let circuit_id = data.circuit.as_deref().unwrap_or("");
        let slice_num = data.slice_num.as_deref().unwrap_or("");

        info!(circuit = circuit_id, slice = slice_num, "handling DSlice");

        let result = self
            .dsperse
            .prove_slice(circuit_id, slice_num, &data.inputs.unwrap_or(json!({})))
            .await?;

        Ok(result)
    }
}
