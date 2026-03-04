use std::sync::Arc;

use anyhow::{Context, Result};
use parity_scale_codec::Encode;
use sp_core::hashing::blake2_256;
use subxt::dynamic::Value;
use subxt::tx::Signer;
use subxt::{OnlineClient, PolkadotConfig};
use tracing::info;

use crate::wallet::Wallet;

pub struct PendingReveal {
    pub uids: Vec<u16>,
    pub values: Vec<u16>,
    pub salt: Vec<u16>,
    pub version_key: u64,
    pub commit_block: u64,
}

#[derive(Clone)]
pub struct WeightsSetter {
    netuid: u16,
}

impl WeightsSetter {
    pub fn new(netuid: u16) -> Self {
        Self { netuid }
    }

    pub fn compute_commit_hash(
        hotkey_account: &subxt::utils::AccountId32,
        netuid: u16,
        uids: &[u16],
        values: &[u16],
        salt: &[u16],
        version_key: u64,
    ) -> [u8; 32] {
        let payload = (hotkey_account.0, netuid, uids, values, salt, version_key).encode();
        blake2_256(&payload)
    }

    pub async fn query_tempo(&self, client: &OnlineClient<PolkadotConfig>) -> Result<u64> {
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            "Tempo",
            vec![Value::from(self.netuid as u64)],
        );
        let storage = client.storage().at_latest().await?;
        match storage.fetch(&query).await? {
            Some(val) => Ok(val.to_value()?.as_u128().unwrap_or(360) as u64),
            None => Ok(360),
        }
    }

    pub async fn query_reveal_period(&self, client: &OnlineClient<PolkadotConfig>) -> Result<u64> {
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            "RevealPeriodEpochs",
            vec![Value::from(self.netuid as u64)],
        );
        let storage = client.storage().at_latest().await?;
        match storage.fetch(&query).await? {
            Some(val) => Ok(val.to_value()?.as_u128().unwrap_or(1) as u64),
            None => Ok(1),
        }
    }

    pub async fn current_block(&self, client: &OnlineClient<PolkadotConfig>) -> Result<u64> {
        Ok(client.blocks().at_latest().await?.number() as u64)
    }

    pub fn get_reveal_blocks(
        netuid: u16,
        tempo: u64,
        reveal_period: u64,
        commit_block: u64,
    ) -> (u64, u64) {
        let tempo_plus_one = tempo + 1;
        let netuid_offset = netuid as u64 + 1;
        let commit_epoch = (commit_block + netuid_offset) / tempo_plus_one;
        let reveal_epoch = commit_epoch + reveal_period;
        let first_reveal_block = reveal_epoch * tempo_plus_one - netuid_offset;
        let last_reveal_block = first_reveal_block + tempo;
        (first_reveal_block, last_reveal_block)
    }

    pub async fn commit_weights(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        wallet: &Arc<Wallet>,
        commit_hash: &[u8; 32],
    ) -> Result<u64> {
        let tx = subxt::dynamic::tx(
            "SubtensorModule",
            "commit_weights",
            vec![
                Value::from(self.netuid as u64),
                Value::from_bytes(commit_hash),
            ],
        );

        let signer = SubxtSr25519Signer::new(wallet)?;

        let result = client
            .tx()
            .sign_and_submit_then_watch_default(&tx, &signer)
            .await
            .context("submitting commit_weights")?
            .wait_for_finalized_success()
            .await
            .context("commit_weights finalization")?;

        let block = client.blocks().at_latest().await?.number() as u64;
        info!(block = block, hash = %result.extrinsic_hash(), "weights committed");
        Ok(block)
    }

    pub async fn reveal_weights(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        wallet: &Arc<Wallet>,
        uids: &[u16],
        values: &[u16],
        salt: &[u16],
        version_key: u64,
    ) -> Result<()> {
        anyhow::ensure!(
            uids.len() == values.len(),
            "reveal_weights: length mismatch uids={} values={}",
            uids.len(),
            values.len(),
        );

        let uid_vals: Vec<Value> = uids.iter().map(|&u| Value::from(u as u64)).collect();
        let weight_vals: Vec<Value> = values.iter().map(|&w| Value::from(w as u64)).collect();
        let salt_vals: Vec<Value> = salt.iter().map(|&s| Value::from(s as u64)).collect();

        let tx = subxt::dynamic::tx(
            "SubtensorModule",
            "reveal_weights",
            vec![
                Value::from(self.netuid as u64),
                Value::unnamed_composite(uid_vals),
                Value::unnamed_composite(weight_vals),
                Value::unnamed_composite(salt_vals),
                Value::from(version_key),
            ],
        );

        let signer = SubxtSr25519Signer::new(wallet)?;

        let result = client
            .tx()
            .sign_and_submit_then_watch_default(&tx, &signer)
            .await
            .context("submitting reveal_weights")?
            .wait_for_finalized_success()
            .await
            .context("reveal_weights finalization")?;

        info!(block = %result.extrinsic_hash(), "weights revealed");
        Ok(())
    }

    pub async fn blocks_since_last_update(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        uid: u16,
    ) -> Result<u64> {
        let query = subxt::dynamic::storage(
            "SubtensorModule",
            "LastUpdate",
            vec![Value::from(self.netuid as u64)],
        );

        let storage = client.storage().at_latest().await?;
        let result = storage.fetch(&query).await?;

        let last_update = match result {
            Some(val) => {
                let updates: Vec<u64> = val.as_type().context("decoding LastUpdate vec")?;
                updates.get(uid as usize).copied().unwrap_or(0)
            }
            None => 0,
        };

        let block = client.blocks().at_latest().await?.number() as u64;
        Ok(block.saturating_sub(last_update))
    }
}

pub(crate) struct SubxtSr25519Signer {
    wallet: Arc<Wallet>,
    account_id: subxt::utils::AccountId32,
}

impl SubxtSr25519Signer {
    pub(crate) fn new(wallet: &Arc<Wallet>) -> Result<Self> {
        let account_id = wallet.hotkey_account_id()?;
        Ok(Self {
            wallet: Arc::clone(wallet),
            account_id,
        })
    }
}

impl Signer<PolkadotConfig> for SubxtSr25519Signer {
    fn account_id(&self) -> subxt::utils::AccountId32 {
        self.account_id.clone()
    }

    fn address(&self) -> <PolkadotConfig as subxt::Config>::Address {
        self.account_id.clone().into()
    }

    fn sign(&self, payload: &[u8]) -> <PolkadotConfig as subxt::Config>::Signature {
        let sig = self.wallet.sign_hotkey(payload).expect("signing failed");
        let sig_arr: [u8; 64] = sig.try_into().expect("signature not 64 bytes");
        subxt::utils::MultiSignature::Sr25519(sig_arr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_reveal_blocks_basic_calculation() {
        let commit_block = 1000u64;
        let (first, last) = WeightsSetter::get_reveal_blocks(2, 360, 1, commit_block);
        assert_eq!(first, 1080);
        assert_eq!(last, 1440);
        assert!(
            commit_block < first,
            "commit block must precede reveal window"
        );
    }

    #[test]
    fn compute_commit_hash_deterministic() {
        let account = subxt::utils::AccountId32::from([1u8; 32]);
        let h1 = WeightsSetter::compute_commit_hash(&account, 2, &[1, 2], &[100, 200], &[42], 1);
        let h2 = WeightsSetter::compute_commit_hash(&account, 2, &[1, 2], &[100, 200], &[42], 1);
        assert_eq!(h1, h2);
    }

    #[test]
    fn compute_commit_hash_varies_with_each_field() {
        let account = subxt::utils::AccountId32::from([1u8; 32]);
        let account2 = subxt::utils::AccountId32::from([2u8; 32]);
        let base = WeightsSetter::compute_commit_hash(&account, 2, &[1, 2], &[100, 200], &[42], 1);

        let cases: Vec<[u8; 32]> = vec![
            WeightsSetter::compute_commit_hash(&account2, 2, &[1, 2], &[100, 200], &[42], 1),
            WeightsSetter::compute_commit_hash(&account, 3, &[1, 2], &[100, 200], &[42], 1),
            WeightsSetter::compute_commit_hash(&account, 2, &[1, 3], &[100, 200], &[42], 1),
            WeightsSetter::compute_commit_hash(&account, 2, &[1, 2], &[100, 201], &[42], 1),
            WeightsSetter::compute_commit_hash(&account, 2, &[1, 2], &[100, 200], &[99], 1),
            WeightsSetter::compute_commit_hash(&account, 2, &[1, 2], &[100, 200], &[42], 2),
        ];
        for (i, h) in cases.iter().enumerate() {
            assert_ne!(&base, h, "variation {i} should produce a different hash");
        }
    }
}
