use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use btlightning::{LightningServer, SynapseHandler};
use tracing::info;

use sn2_types::*;

use crate::handlers::MinerHandlers;

fn rmpv_to_json(data: HashMap<String, rmpv::Value>) -> btlightning::Result<serde_json::Value> {
    let map: serde_json::Map<String, serde_json::Value> = data
        .into_iter()
        .map(|(k, v)| serde_json::to_value(v).map(|jv| (k, jv)))
        .collect::<serde_json::Result<_>>()
        .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
    Ok(serde_json::Value::Object(map))
}

fn json_to_rmpv(val: serde_json::Value) -> btlightning::Result<HashMap<String, rmpv::Value>> {
    match val {
        serde_json::Value::Object(map) => map
            .into_iter()
            .map(|(k, v)| rmpv::ext::to_value(v).map(|rv| (k, rv)))
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| btlightning::LightningError::Handler(e.to_string())),
        _ => Ok(HashMap::new()),
    }
}

struct QueryZkProofHandler {
    handlers: Arc<MinerHandlers>,
    rt: tokio::runtime::Handle,
}

impl SynapseHandler for QueryZkProofHandler {
    fn handle(
        &self,
        _synapse_type: &str,
        data: HashMap<String, rmpv::Value>,
    ) -> btlightning::Result<HashMap<String, rmpv::Value>> {
        let query: QueryZkProof = serde_json::from_value(rmpv_to_json(data)?)
            .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        let result = tokio::task::block_in_place(|| {
            self.rt.block_on(self.handlers.handle_query_zk_proof(query))
        })
        .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        json_to_rmpv(result)
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
        data: HashMap<String, rmpv::Value>,
    ) -> btlightning::Result<HashMap<String, rmpv::Value>> {
        let query: DSliceProofGenerationDataModel = serde_json::from_value(rmpv_to_json(data)?)
            .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        let result =
            tokio::task::block_in_place(|| self.rt.block_on(self.handlers.handle_dslice(query)))
                .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        json_to_rmpv(result)
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
        data: HashMap<String, rmpv::Value>,
    ) -> btlightning::Result<HashMap<String, rmpv::Value>> {
        let query: Competition = serde_json::from_value(rmpv_to_json(data)?)
            .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        let result = tokio::task::block_in_place(|| {
            self.rt.block_on(self.handlers.handle_competition(query))
        })
        .map_err(|e| btlightning::LightningError::Handler(e.to_string()))?;
        json_to_rmpv(result)
    }
}

pub async fn run_lightning_server(
    miner_hotkey: &str,
    wallet_name: &str,
    wallet_path: &str,
    hotkey_name: &str,
    host: &str,
    port: u16,
    handlers: Arc<MinerHandlers>,
) -> Result<()> {
    let rt = tokio::runtime::Handle::current();

    let mut server = LightningServer::new(miner_hotkey.to_string(), host.to_string(), port);

    server
        .set_miner_wallet(wallet_name, wallet_path, hotkey_name)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

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
