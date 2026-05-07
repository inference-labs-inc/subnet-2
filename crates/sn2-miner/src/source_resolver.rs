use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;

use btlightning::{LightningError, Result as LightningResult, SourceAddressResolver};
use sn2_chain::Metagraph;
use tokio::sync::RwLock;

pub struct MetagraphSourceResolver {
    metagraph: Arc<RwLock<Metagraph>>,
}

impl MetagraphSourceResolver {
    pub fn new(metagraph: Arc<RwLock<Metagraph>>) -> Self {
        Self { metagraph }
    }
}

impl SourceAddressResolver for MetagraphSourceResolver {
    fn resolve_allowed_sources(&self) -> LightningResult<HashSet<IpAddr>> {
        let guard = self.metagraph.blocking_read();
        if guard.neurons.is_empty() {
            return Err(LightningError::Handler(
                "metagraph has not been synced; refusing to resolve empty source allowlist"
                    .to_string(),
            ));
        }
        Ok(guard
            .neurons
            .iter()
            .filter(|n| n.validator_permit)
            .filter(|n| !n.axon_ip.is_empty())
            .filter_map(|n| n.axon_ip.parse::<IpAddr>().ok())
            .collect())
    }
}
