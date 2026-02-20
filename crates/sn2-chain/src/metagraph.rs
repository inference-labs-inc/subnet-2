use std::collections::HashMap;

use anyhow::{Context, Result};
use sp_core::crypto::Ss58Codec;
use subxt::dynamic::Value;
use subxt::ext::scale_value::At;
use subxt::{OnlineClient, PolkadotConfig};
use tracing::{info, warn};

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

    pub async fn sync(&mut self, client: &OnlineClient<PolkadotConfig>) -> Result<()> {
        let block = client
            .blocks()
            .at_latest()
            .await
            .context("fetching latest block")?;
        self.block = block.number() as u64;

        let n = self.query_subnet_n(client).await?;
        self.n = n;

        info!(
            netuid = self.netuid,
            n = n,
            block = self.block,
            "syncing metagraph"
        );

        let validator_permits = self.query_validator_permits(client).await.map_err(|e| {
            warn!(
                netuid = self.netuid,
                error = %e,
                "validator permits not updated"
            );
            e
        })?;

        let mut neurons = Vec::with_capacity(n as usize);

        for uid in 0..n {
            match self.query_neuron(client, uid).await {
                Ok(mut neuron) => {
                    neuron.validator_permit = validator_permits
                        .get(uid as usize)
                        .copied()
                        .unwrap_or(false);
                    neurons.push(neuron);
                }
                Err(e) => {
                    warn!(uid = uid, error = %e, "skipping neuron");
                }
            }
        }

        self.uid_to_idx.clear();
        self.hotkey_to_uid.clear();
        self.coldkey_to_uids.clear();

        for (idx, neuron) in neurons.iter().enumerate() {
            self.uid_to_idx.insert(neuron.uid, idx);
            self.hotkey_to_uid.insert(neuron.hotkey.clone(), neuron.uid);
            self.coldkey_to_uids
                .entry(neuron.coldkey.clone())
                .or_default()
                .push(neuron.uid);
        }

        self.neurons = neurons;
        info!(
            netuid = self.netuid,
            neurons = self.neurons.len(),
            "metagraph synced"
        );
        Ok(())
    }

    async fn query_subnet_n(&self, client: &OnlineClient<PolkadotConfig>) -> Result<u16> {
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            "SubnetworkN",
            vec![Value::from(self.netuid as u64)],
        );

        let result = client.storage().at_latest().await?.fetch(&query).await?;

        match result {
            Some(val) => {
                let n = val.to_value()?.as_u128().context("SubnetworkN not u128")? as u16;
                Ok(n)
            }
            None => Ok(0),
        }
    }

    async fn query_neuron(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        uid: u16,
    ) -> Result<NeuronInfo> {
        let (hotkey_bytes, hotkey) = self.query_hotkey(client, uid).await?;
        let coldkey = self
            .query_coldkey(client, &hotkey_bytes)
            .await
            .unwrap_or_default();
        let stake = self.query_stake(client, &hotkey_bytes).await.unwrap_or(0);
        let rank = self
            .query_u16_storage(client, "Rank", uid)
            .await
            .unwrap_or(0);
        let trust = self
            .query_u16_storage(client, "Trust", uid)
            .await
            .unwrap_or(0);
        let consensus = self
            .query_u16_storage(client, "Consensus", uid)
            .await
            .unwrap_or(0);
        let incentive = self
            .query_u16_storage(client, "Incentive", uid)
            .await
            .unwrap_or(0);
        let dividends = self
            .query_u16_storage(client, "Dividends", uid)
            .await
            .unwrap_or(0);
        let emission = self
            .query_u64_storage(client, "Emission", uid)
            .await
            .unwrap_or(0);
        let is_active = self
            .query_bool_storage(client, "Active", uid)
            .await
            .unwrap_or(false);
        let last_update = self
            .query_u64_storage(client, "LastUpdate", uid)
            .await
            .unwrap_or(0);
        let (axon_ip, axon_port, axon_protocol) = self
            .query_axon(client, &hotkey_bytes)
            .await
            .unwrap_or_default();

        Ok(NeuronInfo {
            uid,
            hotkey,
            coldkey,
            hotkey_bytes,
            stake,
            rank,
            trust,
            consensus,
            incentive,
            dividends,
            emission,
            is_active,
            last_update,
            axon_ip,
            axon_port,
            axon_protocol,
            validator_permit: false,
        })
    }

    async fn query_hotkey(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        uid: u16,
    ) -> Result<([u8; 32], String)> {
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            "Keys",
            vec![Value::from(self.netuid as u64), Value::from(uid as u64)],
        );

        let result = client
            .storage()
            .at_latest()
            .await?
            .fetch(&query)
            .await?
            .context("hotkey not found")?;

        let account_id: subxt::utils::AccountId32 = result.as_type()?;
        let bytes = account_id.0;
        let ss58 = sp_core::crypto::AccountId32::new(bytes).to_ss58check();
        Ok((bytes, ss58))
    }

    async fn query_coldkey(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        hotkey_bytes: &[u8; 32],
    ) -> Result<String> {
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            "Owner",
            vec![Value::from_bytes(hotkey_bytes)],
        );

        let result = client
            .storage()
            .at_latest()
            .await?
            .fetch(&query)
            .await?
            .context("coldkey not found")?;

        let account_id: subxt::utils::AccountId32 = result.as_type()?;
        let ss58 = sp_core::crypto::AccountId32::new(account_id.0).to_ss58check();
        Ok(ss58)
    }

    async fn query_stake(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        hotkey_bytes: &[u8; 32],
    ) -> Result<u64> {
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            "TotalHotkeyStake",
            vec![Value::from_bytes(hotkey_bytes)],
        );

        let result = client.storage().at_latest().await?.fetch(&query).await?;

        match result {
            Some(val) => Ok(val.to_value()?.as_u128().unwrap_or(0) as u64),
            None => Ok(0),
        }
    }

    async fn query_u16_storage(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        storage_name: &str,
        uid: u16,
    ) -> Result<u16> {
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            storage_name,
            vec![Value::from(self.netuid as u64), Value::from(uid as u64)],
        );

        let result = client.storage().at_latest().await?.fetch(&query).await?;

        match result {
            Some(val) => Ok(val.to_value()?.as_u128().unwrap_or(0) as u16),
            None => Ok(0),
        }
    }

    async fn query_u64_storage(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        storage_name: &str,
        uid: u16,
    ) -> Result<u64> {
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            storage_name,
            vec![Value::from(self.netuid as u64), Value::from(uid as u64)],
        );

        let result = client.storage().at_latest().await?.fetch(&query).await?;

        match result {
            Some(val) => Ok(val.to_value()?.as_u128().unwrap_or(0) as u64),
            None => Ok(0),
        }
    }

    async fn query_bool_storage(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        storage_name: &str,
        uid: u16,
    ) -> Result<bool> {
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            storage_name,
            vec![Value::from(self.netuid as u64), Value::from(uid as u64)],
        );

        let result = client.storage().at_latest().await?.fetch(&query).await?;

        match result {
            Some(val) => Ok(val.to_value()?.as_bool().unwrap_or(false)),
            None => Ok(false),
        }
    }

    async fn query_axon(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        hotkey_bytes: &[u8; 32],
    ) -> Result<(String, u16, u8)> {
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            "Axons",
            vec![
                Value::from(self.netuid as u64),
                Value::from_bytes(hotkey_bytes),
            ],
        );

        let result = client.storage().at_latest().await?.fetch(&query).await?;

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

    async fn query_validator_permits(
        &self,
        client: &OnlineClient<PolkadotConfig>,
    ) -> Result<Vec<bool>> {
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            "ValidatorPermit",
            vec![Value::from(self.netuid as u64)],
        );

        let result = client.storage().at_latest().await?.fetch(&query).await?;

        match result {
            Some(val) => val
                .as_type::<Vec<bool>>()
                .context("decoding ValidatorPermit"),
            None => Ok(Vec::new()),
        }
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
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            "SubnetOwner",
            vec![Value::from(self.netuid as u64)],
        );

        let result = client.storage().at_latest().await?.fetch(&query).await?;

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
