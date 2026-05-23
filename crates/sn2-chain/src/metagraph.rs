use std::collections::HashMap;
use std::net::Ipv4Addr;

use anyhow::{Context, Result};
use futures_util::stream::{self, StreamExt};
use parity_scale_codec::{Compact, Decode, Encode};
use sp_core::crypto::Ss58Codec;
use subxt::dynamic::Value;
use subxt::ext::scale_value::At;
use subxt::{OnlineClient, OnlineClientAtBlock, PolkadotConfig};
use tracing::{debug, info, warn};

use crate::subxt_helpers::{
    at_current_block, fetch_typed, fetch_u128_or, fetch_value, netuid_hotkey_keys, netuid_keys,
};

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
    /// Per-subnet consensus stake-coverage threshold, encoded as `numerator / u16::MAX`.
    /// Populated by [`sync`](Self::sync); 0 until the first successful sync.
    pub kappa: u16,
    /// Per-subnet epoch length in blocks. Populated by [`sync`](Self::sync); 0 until the
    /// first successful sync.
    pub tempo: u16,
    uid_to_idx: HashMap<u16, usize>,
    hotkey_to_uid: HashMap<String, u16>,
    coldkey_to_uids: HashMap<String, Vec<u16>>,
}

impl Metagraph {
    /// Returns `kappa` as a fraction in `[0.0, 1.0]`.
    pub fn kappa_fraction(&self) -> f64 {
        self.kappa as f64 / u16::MAX as f64
    }
}

impl Metagraph {
    pub fn new(netuid: u16) -> Self {
        Self {
            netuid,
            neurons: Vec::new(),
            n: 0,
            block: 0,
            kappa: 0,
            tempo: 0,
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
            kappa: 0,
            tempo: 0,
            uid_to_idx,
            hotkey_to_uid,
            coldkey_to_uids,
        }
    }

    pub async fn sync(&mut self, client: &OnlineClient<PolkadotConfig>) -> Result<()> {
        let at_block = at_current_block(client).await?;
        self.block = at_block.block_number();

        let n = query_subnet_n(&at_block, self.netuid).await?;
        self.n = n;

        self.kappa = fetch_typed::<u16>(&at_block, "Kappa", netuid_keys(self.netuid))
            .await?
            .unwrap_or(0);
        self.tempo = fetch_typed::<u16>(&at_block, "Tempo", netuid_keys(self.netuid))
            .await?
            .unwrap_or(0);

        info!(
            netuid = self.netuid,
            n = n,
            block = self.block,
            kappa = self.kappa,
            tempo = self.tempo,
            "syncing metagraph"
        );

        let neurons = match self.sync_via_runtime_api(&at_block).await {
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
                self.sync_via_storage(&at_block, n).await?
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
        at_block: &OnlineClientAtBlock<PolkadotConfig>,
    ) -> Result<Vec<NeuronInfo>> {
        let params = Encode::encode(&self.netuid);
        let bytes = at_block
            .runtime_apis()
            .call_raw("NeuronInfoRuntimeApi_get_neurons_lite", Some(&params))
            .await
            .context("calling get_neurons_lite")?;
        let items: Vec<NeuronInfoLiteRaw> =
            Decode::decode(&mut bytes.as_slice()).context("decoding get_neurons_lite response")?;

        let neurons: Vec<NeuronInfo> = items
            .into_iter()
            .map(|raw| {
                let hotkey = sp_core::crypto::AccountId32::new(raw.hotkey).to_ss58check();
                let coldkey = sp_core::crypto::AccountId32::new(raw.coldkey).to_ss58check();
                let total_stake: u64 = raw.stake.iter().map(|(_, s)| s.0).sum();

                let axon_ip = if raw.axon_info.ip_type == 4 {
                    Ipv4Addr::from(raw.axon_info.ip as u32).to_string()
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
        at_block: &OnlineClientAtBlock<PolkadotConfig>,
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
        ) = tokio::try_join!(
            query_vec::<bool>(at_block, "ValidatorPermit", netuid),
            query_vec::<u16>(at_block, "Rank", netuid),
            query_vec::<u16>(at_block, "Trust", netuid),
            query_vec::<u16>(at_block, "Consensus", netuid),
            query_vec::<u16>(at_block, "Incentive", netuid),
            query_vec::<u16>(at_block, "Dividends", netuid),
            query_vec::<u64>(at_block, "Emission", netuid),
            query_vec::<bool>(at_block, "Active", netuid),
            query_vec::<u64>(at_block, "LastUpdate", netuid),
        )?;

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
                    match query_neuron_core(at_block, netuid, uid).await {
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

    pub fn uids(&self) -> Vec<u16> {
        self.neurons.iter().map(|n| n.uid).collect()
    }

    pub fn active_neurons(&self) -> impl Iterator<Item = &NeuronInfo> {
        self.neurons.iter().filter(|n| n.is_active)
    }

    pub async fn query_subnet_owner(
        &self,
        client: &OnlineClient<PolkadotConfig>,
    ) -> Result<Option<u16>> {
        let at_block = at_current_block(client).await?;
        match fetch_typed::<subxt::utils::AccountId32>(
            &at_block,
            "SubnetOwner",
            netuid_keys(self.netuid),
        )
        .await?
        {
            Some(account_id) => {
                let ss58 = sp_core::crypto::AccountId32::new(account_id.0).to_ss58check();
                Ok(self.get_uid_by_coldkey(&ss58))
            }
            None => Ok(None),
        }
    }
}

async fn query_subnet_n(
    at_block: &OnlineClientAtBlock<PolkadotConfig>,
    netuid: u16,
) -> Result<u16> {
    Ok(fetch_u128_or(at_block, "SubnetworkN", netuid_keys(netuid), 0).await? as u16)
}

async fn query_neuron_core(
    at_block: &OnlineClientAtBlock<PolkadotConfig>,
    netuid: u16,
    uid: u16,
) -> Result<NeuronInfo> {
    let (hotkey_bytes, hotkey) = query_hotkey(at_block, netuid, uid).await?;

    let (coldkey, stake, axon) = tokio::try_join!(
        query_coldkey(at_block, &hotkey_bytes),
        query_stake(at_block, &hotkey_bytes),
        query_axon(at_block, netuid, &hotkey_bytes),
    )?;

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
    at_block: &OnlineClientAtBlock<PolkadotConfig>,
    netuid: u16,
    uid: u16,
) -> Result<([u8; 32], String)> {
    let keys = vec![Value::from(netuid as u64), Value::from(uid as u64)];
    let account_id = fetch_typed::<subxt::utils::AccountId32>(at_block, "Keys", keys)
        .await?
        .context("hotkey not found")?;
    let bytes = account_id.0;
    Ok((
        bytes,
        sp_core::crypto::AccountId32::new(bytes).to_ss58check(),
    ))
}

async fn query_coldkey(
    at_block: &OnlineClientAtBlock<PolkadotConfig>,
    hotkey_bytes: &[u8; 32],
) -> Result<String> {
    let account_id = fetch_typed::<subxt::utils::AccountId32>(
        at_block,
        "Owner",
        vec![Value::from_bytes(hotkey_bytes)],
    )
    .await?
    .context("coldkey not found")?;
    Ok(sp_core::crypto::AccountId32::new(account_id.0).to_ss58check())
}

async fn query_stake(
    at_block: &OnlineClientAtBlock<PolkadotConfig>,
    hotkey_bytes: &[u8; 32],
) -> Result<u64> {
    Ok(fetch_u128_or(
        at_block,
        "TotalHotkeyStake",
        vec![Value::from_bytes(hotkey_bytes)],
        0,
    )
    .await? as u64)
}

async fn query_vec<T: subxt::ext::scale_decode::IntoVisitor>(
    at_block: &OnlineClientAtBlock<PolkadotConfig>,
    storage_name: &str,
    netuid: u16,
) -> Result<Vec<T>> {
    fetch_typed::<Vec<T>>(at_block, storage_name, netuid_keys(netuid))
        .await
        .with_context(|| format!("decoding {storage_name} vec"))
        .map(Option::unwrap_or_default)
}

async fn query_axon(
    at_block: &OnlineClientAtBlock<PolkadotConfig>,
    netuid: u16,
    hotkey_bytes: &[u8; 32],
) -> Result<(String, u16, u8)> {
    match fetch_value(at_block, "Axons", netuid_hotkey_keys(netuid, hotkey_bytes)).await? {
        Some(v) => {
            let ip_raw = v.at("ip").and_then(|v| v.as_u128()).unwrap_or(0) as u32;
            let port = v.at("port").and_then(|v| v.as_u128()).unwrap_or(0) as u16;
            let protocol = v.at("protocol").and_then(|v| v.as_u128()).unwrap_or(0) as u8;

            let ip = Ipv4Addr::from(ip_raw).to_string();

            Ok((ip, port, protocol))
        }
        None => Ok((String::new(), 0, 0)),
    }
}
