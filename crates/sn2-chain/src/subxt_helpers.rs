use anyhow::{Context, Result};
use subxt::dynamic::Value;
use subxt::ext::scale_decode::IntoVisitor;
use subxt::{OnlineClient, OnlineClientAtBlock, PolkadotConfig};

pub(crate) const PALLET: &str = "SubtensorModule";

pub(crate) async fn at_current_block(
    client: &OnlineClient<PolkadotConfig>,
) -> Result<OnlineClientAtBlock<PolkadotConfig>> {
    client
        .at_current_block()
        .await
        .context("fetching latest block")
}

pub(crate) async fn fetch_value(
    at_block: &OnlineClientAtBlock<PolkadotConfig>,
    entry: &str,
    keys: Vec<Value>,
) -> Result<Option<Value>> {
    let query = subxt::dynamic::storage(PALLET, entry);
    match at_block.storage().try_fetch(query, keys).await? {
        Some(val) => Ok(Some(val.decode()?)),
        None => Ok(None),
    }
}

pub(crate) async fn fetch_u128_or(
    at_block: &OnlineClientAtBlock<PolkadotConfig>,
    entry: &str,
    keys: Vec<Value>,
    default: u128,
) -> Result<u128> {
    match fetch_value(at_block, entry, keys).await? {
        None => Ok(default),
        Some(v) => v
            .as_u128()
            .with_context(|| format!("storage entry {PALLET}::{entry} did not decode as u128")),
    }
}

pub(crate) async fn fetch_typed<T: IntoVisitor>(
    at_block: &OnlineClientAtBlock<PolkadotConfig>,
    entry: &str,
    keys: Vec<Value>,
) -> Result<Option<T>> {
    let query = subxt::dynamic::storage::<Vec<Value>, Value>(PALLET, entry);
    match at_block.storage().try_fetch(query, keys).await? {
        Some(val) => Ok(Some(val.decode_as::<T>()?)),
        None => Ok(None),
    }
}

pub(crate) fn netuid_keys(netuid: u16) -> Vec<Value> {
    vec![Value::from(netuid as u64)]
}

pub(crate) fn netuid_hotkey_keys(netuid: u16, hotkey_bytes: &[u8]) -> Vec<Value> {
    vec![Value::from(netuid as u64), Value::from_bytes(hotkey_bytes)]
}
