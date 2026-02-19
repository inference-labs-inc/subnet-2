use anyhow::{Context, Result};
use sp_core::sr25519;
use sp_core::Pair;
use subxt::dynamic::Value;
use subxt::tx::Signer;
use subxt::{OnlineClient, PolkadotConfig};
use tracing::info;

use crate::wallet::Wallet;

pub struct WeightsSetter {
    netuid: u16,
}

impl WeightsSetter {
    pub fn new(netuid: u16) -> Self {
        Self { netuid }
    }

    pub async fn set_weights(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        wallet: &Wallet,
        uids: &[u16],
        weights: &[u16],
        version_key: u32,
    ) -> Result<()> {
        let dests: Vec<Value> = uids.iter().map(|&u| Value::from(u as u64)).collect();
        let weight_vals: Vec<Value> = weights.iter().map(|&w| Value::from(w as u64)).collect();

        let tx = subxt::dynamic::tx(
            "SubtensorModule",
            "set_weights",
            vec![
                Value::from(self.netuid as u64),
                Value::unnamed_composite(dests),
                Value::unnamed_composite(weight_vals),
                Value::from(version_key as u64),
            ],
        );

        let signer = SubxtSr25519Signer::new(wallet.hotkey.clone());

        let result = client
            .tx()
            .sign_and_submit_then_watch_default(&tx, &signer)
            .await
            .context("submitting set_weights")?
            .wait_for_finalized_success()
            .await
            .context("set_weights finalization")?;

        info!(
            block = %result.extrinsic_hash(),
            uids_count = uids.len(),
            "weights set on chain"
        );

        Ok(())
    }

    pub async fn commit_weights(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        wallet: &Wallet,
        commit_hash: &[u8],
    ) -> Result<()> {
        let tx = subxt::dynamic::tx(
            "SubtensorModule",
            "commit_weights",
            vec![
                Value::from(self.netuid as u64),
                Value::from_bytes(commit_hash),
            ],
        );

        let signer = SubxtSr25519Signer::new(wallet.hotkey.clone());

        let result = client
            .tx()
            .sign_and_submit_then_watch_default(&tx, &signer)
            .await
            .context("submitting commit_weights")?
            .wait_for_finalized_success()
            .await
            .context("commit_weights finalization")?;

        info!(block = %result.extrinsic_hash(), "weights committed");
        Ok(())
    }

    pub async fn reveal_weights(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        wallet: &Wallet,
        uids: &[u16],
        values: &[u16],
        salt: &[u16],
        version_key: u32,
    ) -> Result<()> {
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
                Value::from(version_key as u64),
            ],
        );

        let signer = SubxtSr25519Signer::new(wallet.hotkey.clone());

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
            vec![Value::from(self.netuid as u64), Value::from(uid as u64)],
        );

        let result = client.storage().at_latest().await?.fetch(&query).await?;

        let last_update = match result {
            Some(val) => val.to_value()?.as_u128().unwrap_or(0) as u64,
            None => 0,
        };

        let block = client.blocks().at_latest().await?.number() as u64;
        Ok(block.saturating_sub(last_update))
    }
}

pub(crate) struct SubxtSr25519Signer {
    pair: sr25519::Pair,
    account_id: subxt::utils::AccountId32,
}

impl SubxtSr25519Signer {
    pub(crate) fn new(pair: sr25519::Pair) -> Self {
        let public = pair.public();
        let account_id = subxt::utils::AccountId32::from(public.0);
        Self { pair, account_id }
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
        let sig = self.pair.sign(payload);
        subxt::utils::MultiSignature::Sr25519(sig.0)
    }
}
