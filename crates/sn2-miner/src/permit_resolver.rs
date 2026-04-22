use std::collections::HashSet;
use std::sync::Arc;

use btlightning::{LightningError, Result as LightningResult, ValidatorPermitResolver};
use sn2_chain::Metagraph;
use tokio::sync::RwLock;

pub struct MetagraphPermitResolver {
    metagraph: Arc<RwLock<Metagraph>>,
}

impl MetagraphPermitResolver {
    pub fn new(metagraph: Arc<RwLock<Metagraph>>) -> Self {
        Self { metagraph }
    }
}

impl ValidatorPermitResolver for MetagraphPermitResolver {
    fn resolve_permitted_validators(&self) -> LightningResult<HashSet<String>> {
        let guard = self.metagraph.blocking_read();
        if guard.neurons.is_empty() {
            return Err(LightningError::Handler(
                "metagraph has not been synced; refusing to resolve empty permit set".to_string(),
            ));
        }
        Ok(guard
            .neurons
            .iter()
            .filter(|n| n.validator_permit)
            .map(|n| n.hotkey.clone())
            .collect())
    }
}
