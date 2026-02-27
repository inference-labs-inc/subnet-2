use std::collections::HashMap;

use anyhow::{Context, Result};
use futures_util::stream::{self, StreamExt};
use parity_scale_codec::{Compact, Decode, Encode};
use sp_core::crypto::Ss58Codec;
use subxt::dynamic::Value;
use subxt::ext::scale_value::At;
use subxt::storage::Storage;
use subxt::{OnlineClient, PolkadotConfig};
use tracing::{debug, info, warn};

const METAGRAPH_SYNC_CONCURRENCY: usize = 32;

type IndexMaps = (
    HashMap<u16, usize>,
    HashMap<String, u16>,
    HashMap<String, Vec<u16>>,
);

#[derive(Decode)]
struct AxonInfoRaw {
    _block: u64,
    _version: u32,
    ip: u128,
    port: u16,
    ip_type: u8,
    protocol: u8,
    _placeholder1: u8,
    _placeholder2: u8,
}

#[derive(Decode)]
struct PrometheusInfoRaw {
    _block: u64,
    _version: u32,
    _ip: u128,
    _port: u16,
    _ip_type: u8,
}

#[derive(Decode)]
struct NeuronInfoLiteRaw {
    hotkey: [u8; 32],
    coldkey: [u8; 32],
    uid: Compact<u16>,
    _netuid: Compact<u16>,
    active: bool,
    axon_info: AxonInfoRaw,
    _prometheus_info: PrometheusInfoRaw,
    stake: Vec<([u8; 32], Compact<u64>)>,
    rank: Compact<u16>,
    emission: Compact<u64>,
    incentive: Compact<u16>,
    consensus: Compact<u16>,
    trust: Compact<u16>,
    _validator_trust: Compact<u16>,
    dividends: Compact<u16>,
    last_update: Compact<u64>,
    validator_permit: bool,
    _pruning_score: Compact<u16>,
}

#[derive(Debug, Clone)]
pub struct NeuronInfo {
    pub uid: u16,
    pub hotkey: String,
    pub coldkey: String,
    pub hotkey_bytes: [u8; 32],
    pub stake: u64,
    pub rank: u16,
    pub trust: u16,
    pub consensus: u16,
    pub incentive: u16,
    pub dividends: u16,
    pub emission: u64,
    pub is_active: bool,
    pub last_update: u64,
    pub axon_ip: String,
    pub axon_port: u16,
    pub axon_protocol: u8,
    pub validator_permit: bool,
}

pub struct Metagraph {
    pub netuid: u16,
    pub neurons: Vec<NeuronInfo>,
    pub n: u16,
    pub block: u64,
    uid_to_idx: HashMap<u16, usize>,
    hotkey_to_uid: HashMap<String, u16>,
    coldkey_to_uids: HashMap<String, Vec<u16>>,
}

impl Metagraph {
    pub fn new(netuid: u16) -> Self {
        Self {
            netuid,
            neurons: Vec::new(),
            n: 0,
            block: 0,
            uid_to_idx: HashMap::new(),
            hotkey_to_uid: HashMap::new(),
            coldkey_to_uids: HashMap::new(),
        }
    }

    fn build_index_maps(neurons: &[NeuronInfo]) -> IndexMaps {
        let mut uid_to_idx = HashMap::new();
        let mut hotkey_to_uid = HashMap::new();
        let mut coldkey_to_uids: HashMap<String, Vec<u16>> = HashMap::new();
        for (idx, neuron) in neurons.iter().enumerate() {
            uid_to_idx.insert(neuron.uid, idx);
            hotkey_to_uid.insert(neuron.hotkey.clone(), neuron.uid);
            coldkey_to_uids
                .entry(neuron.coldkey.clone())
                .or_default()
                .push(neuron.uid);
        }
        (uid_to_idx, hotkey_to_uid, coldkey_to_uids)
    }

    pub fn from_neurons(netuid: u16, neurons: Vec<NeuronInfo>) -> Self {
        let n = neurons.len() as u16;
        let (uid_to_idx, hotkey_to_uid, coldkey_to_uids) = Self::build_index_maps(&neurons);
        Self {
            netuid,
            neurons,
            n,
            block: 0,
            uid_to_idx,
            hotkey_to_uid,
            coldkey_to_uids,
        }
    }

    pub async fn sync(&mut self, client: &OnlineClient<PolkadotConfig>) -> Result<()> {
        let block_ref = client
            .blocks()
            .at_latest()
            .await
            .context("fetching latest block")?;
        self.block = block_ref.number() as u64;

        let storage = client
            .storage()
            .at_latest()
            .await
            .context("fetching storage at latest block")?;

        let n = query_subnet_n(&storage, self.netuid).await?;
        self.n = n;

        info!(
            netuid = self.netuid,
            n = n,
            block = self.block,
            "syncing metagraph"
        );

        let neurons = match self.sync_via_runtime_api(client).await {
            Ok(neurons) => {
                info!(
                    netuid = self.netuid,
                    neurons = neurons.len(),
                    "synced via runtime API"
                );
                neurons
            }
            Err(e) => {
                warn!(
                    netuid = self.netuid,
                    error = %e,
                    "runtime API unavailable, falling back to storage queries"
                );
                self.sync_via_storage(&storage, n).await?
            }
        };

        let (uid_to_idx, hotkey_to_uid, coldkey_to_uids) = Self::build_index_maps(&neurons);
        self.uid_to_idx = uid_to_idx;
        self.hotkey_to_uid = hotkey_to_uid;
        self.coldkey_to_uids = coldkey_to_uids;
        self.neurons = neurons;
        info!(
            netuid = self.netuid,
            neurons = self.neurons.len(),
            "metagraph synced"
        );
        Ok(())
    }

    async fn sync_via_runtime_api(
        &self,
        client: &OnlineClient<PolkadotConfig>,
    ) -> Result<Vec<NeuronInfo>> {
        let params = Encode::encode(&self.netuid);
        let items: Vec<NeuronInfoLiteRaw> = client
            .runtime_api()
            .at_latest()
            .await?
            .call_raw("NeuronInfoRuntimeApi_get_neurons_lite", Some(&params))
            .await
            .context("calling get_neurons_lite")?;

        let neurons: Vec<NeuronInfo> = items
            .into_iter()
            .map(|raw| {
                let hotkey = sp_core::crypto::AccountId32::new(raw.hotkey).to_ss58check();
                let coldkey = sp_core::crypto::AccountId32::new(raw.coldkey).to_ss58check();
                let total_stake: u64 = raw.stake.iter().map(|(_, s)| s.0).sum();

                let ip_raw = raw.axon_info.ip as u32;
                let axon_ip = if raw.axon_info.ip_type == 4 {
                    format!(
                        "{}.{}.{}.{}",
                        (ip_raw >> 24) & 0xFF,
                        (ip_raw >> 16) & 0xFF,
                        (ip_raw >> 8) & 0xFF,
                        ip_raw & 0xFF,
                    )
                } else {
                    String::new()
                };

                NeuronInfo {
                    uid: raw.uid.0,
                    hotkey,
                    coldkey,
                    hotkey_bytes: raw.hotkey,
                    stake: total_stake,
                    rank: raw.rank.0,
                    trust: raw.trust.0,
                    consensus: raw.consensus.0,
                    incentive: raw.incentive.0,
                    dividends: raw.dividends.0,
                    emission: raw.emission.0,
                    is_active: raw.active,
                    last_update: raw.last_update.0,
                    axon_ip,
                    axon_port: raw.axon_info.port,
                    axon_protocol: raw.axon_info.protocol,
                    validator_permit: raw.validator_permit,
                }
            })
            .collect();

        Ok(neurons)
    }

    async fn sync_via_storage(
        &self,
        storage: &Storage<PolkadotConfig, OnlineClient<PolkadotConfig>>,
        n: u16,
    ) -> Result<Vec<NeuronInfo>> {
        let netuid = self.netuid;
        let (
            validator_permits,
            ranks,
            trusts,
            consensuses,
            incentives,
            dividends_vec,
            emissions,
            actives,
            last_updates,
        ) = tokio::join!(
            query_vec::<bool>(storage, "ValidatorPermit", netuid),
            query_vec::<u16>(storage, "Rank", netuid),
            query_vec::<u16>(storage, "Trust", netuid),
            query_vec::<u16>(storage, "Consensus", netuid),
            query_vec::<u16>(storage, "Incentive", netuid),
            query_vec::<u16>(storage, "Dividends", netuid),
            query_vec::<u64>(storage, "Emission", netuid),
            query_vec::<bool>(storage, "Active", netuid),
            query_vec::<u64>(storage, "LastUpdate", netuid),
        );

        let validator_permits = validator_permits.unwrap_or_default();
        let ranks = ranks.unwrap_or_default();
        let trusts = trusts.unwrap_or_default();
        let consensuses = consensuses.unwrap_or_default();
        let incentives = incentives.unwrap_or_default();
        let dividends_vec = dividends_vec.unwrap_or_default();
        let emissions = emissions.unwrap_or_default();
        let actives = actives.unwrap_or_default();
        let last_updates = last_updates.unwrap_or_default();

        let neurons: Vec<Option<NeuronInfo>> = stream::iter(0..n)
            .map(|uid| {
                let validator_permits = &validator_permits;
                let ranks = &ranks;
                let trusts = &trusts;
                let consensuses = &consensuses;
                let incentives = &incentives;
                let dividends_vec = &dividends_vec;
                let emissions = &emissions;
                let actives = &actives;
                let last_updates = &last_updates;
                async move {
                    let idx = uid as usize;
                    match query_neuron_core(storage, netuid, uid).await {
                        Ok(mut neuron) => {
                            neuron.validator_permit =
                                validator_permits.get(idx).copied().unwrap_or(false);
                            neuron.rank = ranks.get(idx).copied().unwrap_or(0);
                            neuron.trust = trusts.get(idx).copied().unwrap_or(0);
                            neuron.consensus = consensuses.get(idx).copied().unwrap_or(0);
                            neuron.incentive = incentives.get(idx).copied().unwrap_or(0);
                            neuron.dividends = dividends_vec.get(idx).copied().unwrap_or(0);
                            neuron.emission = emissions.get(idx).copied().unwrap_or(0);
                            neuron.is_active = actives.get(idx).copied().unwrap_or(false);
                            neuron.last_update = last_updates.get(idx).copied().unwrap_or(0);
                            debug!(uid = uid, hotkey = %neuron.hotkey, "synced neuron");
                            Some(neuron)
                        }
                        Err(e) => {
                            warn!(uid = uid, error = %e, "skipping neuron");
                            None
                        }
                    }
                }
            })
            .buffer_unordered(METAGRAPH_SYNC_CONCURRENCY)
            .collect()
            .await;

        Ok(neurons.into_iter().flatten().collect())
    }

    pub fn get_neuron(&self, uid: u16) -> Option<&NeuronInfo> {
        self.uid_to_idx.get(&uid).map(|&idx| &self.neurons[idx])
    }

    pub fn get_uid_by_hotkey(&self, hotkey: &str) -> Option<u16> {
        self.hotkey_to_uid.get(hotkey).copied()
    }

    pub fn get_uid_by_coldkey(&self, coldkey: &str) -> Option<u16> {
        self.coldkey_to_uids
            .get(coldkey)
            .and_then(|uids| uids.first().copied())
    }

    pub fn get_uids_by_coldkey(&self, coldkey: &str) -> &[u16] {
        self.coldkey_to_uids
            .get(coldkey)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn uids(&self) -> Vec<u16> {
        self.neurons.iter().map(|n| n.uid).collect()
    }

    pub fn hotkeys(&self) -> Vec<&str> {
        self.neurons.iter().map(|n| n.hotkey.as_str()).collect()
    }

    pub fn stakes(&self) -> Vec<u64> {
        self.neurons.iter().map(|n| n.stake).collect()
    }

    pub fn active_neurons(&self) -> impl Iterator<Item = &NeuronInfo> {
        self.neurons.iter().filter(|n| n.is_active)
    }

    pub async fn query_subnet_owner(
        &self,
        client: &OnlineClient<PolkadotConfig>,
    ) -> Result<Option<u16>> {
        let storage = client.storage().at_latest().await?;
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            "SubnetOwner",
            vec![Value::from(self.netuid as u64)],
        );

        let result = storage.fetch(&query).await?;

        match result {
            Some(val) => {
                let account_id: subxt::utils::AccountId32 = val.as_type()?;
                let ss58 = sp_core::crypto::AccountId32::new(account_id.0).to_ss58check();
                Ok(self.get_uid_by_coldkey(&ss58))
            }
            None => Ok(None),
        }
    }
}

async fn query_subnet_n(
    storage: &Storage<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    netuid: u16,
) -> Result<u16> {
    let query = subxt::dynamic::storage(
        "SubtensorModule",
        "SubnetworkN",
        vec![Value::from(netuid as u64)],
    );

    let result = storage.fetch(&query).await?;

    match result {
        Some(val) => {
            let n = val.to_value()?.as_u128().context("SubnetworkN not u128")? as u16;
            Ok(n)
        }
        None => Ok(0),
    }
}

async fn query_neuron_core(
    storage: &Storage<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    netuid: u16,
    uid: u16,
) -> Result<NeuronInfo> {
    let (hotkey_bytes, hotkey) = query_hotkey(storage, netuid, uid).await?;

    let (coldkey, stake, axon) = tokio::join!(
        async {
            query_coldkey(storage, &hotkey_bytes)
                .await
                .unwrap_or_default()
        },
        async { query_stake(storage, &hotkey_bytes).await.unwrap_or(0) },
        async {
            query_axon(storage, netuid, &hotkey_bytes)
                .await
                .unwrap_or_default()
        },
    );

    Ok(NeuronInfo {
        uid,
        hotkey,
        coldkey,
        hotkey_bytes,
        stake,
        rank: 0,
        trust: 0,
        consensus: 0,
        incentive: 0,
        dividends: 0,
        emission: 0,
        is_active: false,
        last_update: 0,
        axon_ip: axon.0,
        axon_port: axon.1,
        axon_protocol: axon.2,
        validator_permit: false,
    })
}

async fn query_hotkey(
    storage: &Storage<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    netuid: u16,
    uid: u16,
) -> Result<([u8; 32], String)> {
    let query = subxt::dynamic::storage(
        "SubtensorModule",
        "Keys",
        vec![Value::from(netuid as u64), Value::from(uid as u64)],
    );

    let result = storage.fetch(&query).await?.context("hotkey not found")?;

    let account_id: subxt::utils::AccountId32 = result.as_type()?;
    let bytes = account_id.0;
    let ss58 = sp_core::crypto::AccountId32::new(bytes).to_ss58check();
    Ok((bytes, ss58))
}

async fn query_coldkey(
    storage: &Storage<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    hotkey_bytes: &[u8; 32],
) -> Result<String> {
    let query = subxt::dynamic::storage(
        "SubtensorModule",
        "Owner",
        vec![Value::from_bytes(hotkey_bytes)],
    );

    let result = storage.fetch(&query).await?.context("coldkey not found")?;

    let account_id: subxt::utils::AccountId32 = result.as_type()?;
    let ss58 = sp_core::crypto::AccountId32::new(account_id.0).to_ss58check();
    Ok(ss58)
}

async fn query_stake(
    storage: &Storage<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    hotkey_bytes: &[u8; 32],
) -> Result<u64> {
    let query = subxt::dynamic::storage(
        "SubtensorModule",
        "TotalHotkeyStake",
        vec![Value::from_bytes(hotkey_bytes)],
    );

    let result = storage.fetch(&query).await?;

    match result {
        Some(val) => Ok(val.to_value()?.as_u128().unwrap_or(0) as u64),
        None => Ok(0),
    }
}

async fn query_vec<T: subxt::ext::scale_decode::IntoVisitor>(
    storage: &Storage<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    storage_name: &str,
    netuid: u16,
) -> Result<Vec<T>> {
    let query = subxt::dynamic::storage(
        "SubtensorModule",
        storage_name,
        vec![Value::from(netuid as u64)],
    );

    let result = storage.fetch(&query).await?;

    match result {
        Some(val) => val
            .as_type::<Vec<T>>()
            .with_context(|| format!("decoding {storage_name} vec")),
        None => Ok(Vec::new()),
    }
}

async fn query_axon(
    storage: &Storage<PolkadotConfig, OnlineClient<PolkadotConfig>>,
    netuid: u16,
    hotkey_bytes: &[u8; 32],
) -> Result<(String, u16, u8)> {
    let query = subxt::dynamic::storage(
        "SubtensorModule",
        "Axons",
        vec![Value::from(netuid as u64), Value::from_bytes(hotkey_bytes)],
    );

    let result = storage.fetch(&query).await?;

    match result {
        Some(val) => {
            let v = val.to_value()?;
            let ip_raw = v.at("ip").and_then(|v| v.as_u128()).unwrap_or(0) as u32;
            let port = v.at("port").and_then(|v| v.as_u128()).unwrap_or(0) as u16;
            let protocol = v.at("protocol").and_then(|v| v.as_u128()).unwrap_or(0) as u8;

            let ip = format!(
                "{}.{}.{}.{}",
                (ip_raw >> 24) & 0xFF,
                (ip_raw >> 16) & 0xFF,
                (ip_raw >> 8) & 0xFF,
                ip_raw & 0xFF,
            );

            Ok((ip, port, protocol))
        }
        None => Ok((String::new(), 0, 0)),
    }
}
