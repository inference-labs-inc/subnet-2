use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use btlightning::{LightningServer, SynapseHandler};
use serde_json::json;
use tracing::info;

use sn2_types::*;

use crate::handlers::MinerHandlers;

struct QueryZkProofHandler {
    handlers: Arc<MinerHandlers>,
    rt: tokio::runtime::Handle,
}

impl SynapseHandler for QueryZkProofHandler {
    fn handle(
        &self,
        _synapse_type: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> btlightning::Result<HashMap<String, serde_json::Value>> {
        let query: QueryZkProof = serde_json::from_value(json!(data))
            .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        let result = self
            .rt
            .block_on(self.handlers.handle_query_zk_proof(query))
            .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(result)
            .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        Ok(map)
    }
}

struct DSliceHandler {
    handlers: Arc<MinerHandlers>,
    rt: tokio::runtime::Handle,
}

impl SynapseHandler for DSliceHandler {
    fn handle(
        &self,
        _synapse_type: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> btlightning::Result<HashMap<String, serde_json::Value>> {
        let query: DSliceProofGenerationDataModel = serde_json::from_value(json!(data))
            .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        let result = self
            .rt
            .block_on(self.handlers.handle_dslice(query))
            .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(result)
            .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        Ok(map)
    }
}

struct CompetitionHandler {
    handlers: Arc<MinerHandlers>,
    rt: tokio::runtime::Handle,
}

impl SynapseHandler for CompetitionHandler {
    fn handle(
        &self,
        _synapse_type: &str,
        data: HashMap<String, serde_json::Value>,
    ) -> btlightning::Result<HashMap<String, serde_json::Value>> {
        let query: Competition = serde_json::from_value(json!(data))
            .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        let result = self
            .rt
            .block_on(self.handlers.handle_competition(query))
            .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        let map: HashMap<String, serde_json::Value> = serde_json::from_value(result)
            .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        Ok(map)
    }
}

pub async fn run_lightning_server(
    miner_hotkey: &str,
    miner_seed: [u8; 32],
    host: &str,
    port: u16,
    handlers: Arc<MinerHandlers>,
) -> Result<()> {
    let rt = tokio::runtime::Handle::current();

    let mut server = LightningServer::new(miner_hotkey.to_string(), host.to_string(), port);

    server.set_miner_keypair(miner_seed);

    server
        .register_synapse_handler(
            QueryZkProof::NAME.to_string(),
            Arc::new(QueryZkProofHandler {
                handlers: handlers.clone(),
                rt: rt.clone(),
            }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    server
        .register_synapse_handler(
            DSliceProofGenerationDataModel::NAME.to_string(),
            Arc::new(DSliceHandler {
                handlers: handlers.clone(),
                rt: rt.clone(),
            }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    server
        .register_synapse_handler(
            Competition::NAME.to_string(),
            Arc::new(CompetitionHandler { handlers, rt }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    server.start().await.map_err(|e| anyhow::anyhow!("{e}"))?;

    info!(host = host, port = port, "QUIC Lightning server listening");

    server
        .serve_forever()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
