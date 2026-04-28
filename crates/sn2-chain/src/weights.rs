use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use subxt::dynamic::Value;
use subxt::tx::Signer;
use subxt::{OnlineClient, PolkadotConfig};
use tracing::info;

use crate::subxt_helpers::{at_current_block, fetch_typed, fetch_u128_or, netuid_keys, PALLET};
use crate::wallet::Wallet;

const BLOCK_TIME: f64 = 12.0;
const TX_SUBMIT_TIMEOUT: Duration = Duration::from_secs(30);
const TX_FINALIZATION_TIMEOUT: Duration = Duration::from_secs(180);
const COMMIT_REVEAL_VERSION: u64 = 4;

#[derive(Clone)]
pub struct WeightsSetter {
    netuid: u16,
}

impl WeightsSetter {
    pub fn new(netuid: u16) -> Self {
        Self { netuid }
    }

    pub async fn query_tempo(&self, client: &OnlineClient<PolkadotConfig>) -> Result<u64> {
        let at_block = at_current_block(client).await?;
        Ok(fetch_u128_or(&at_block, "Tempo", netuid_keys(self.netuid), 360).await? as u64)
    }

    pub async fn query_reveal_period(&self, client: &OnlineClient<PolkadotConfig>) -> Result<u64> {
        let at_block = at_current_block(client).await?;
        Ok(
            fetch_u128_or(&at_block, "RevealPeriodEpochs", netuid_keys(self.netuid), 1).await?
                as u64,
        )
    }

    pub async fn current_block(&self, client: &OnlineClient<PolkadotConfig>) -> Result<u64> {
        Ok(at_current_block(client).await?.block_number())
    }

    pub async fn query_commit_params(
        &self,
        client: &OnlineClient<PolkadotConfig>,
    ) -> Result<(u64, u64, u64)> {
        let at_block = at_current_block(client).await?;
        let keys = netuid_keys(self.netuid);
        let (tempo, reveal_period) = tokio::try_join!(
            fetch_u128_or(&at_block, "Tempo", keys.clone(), 360),
            fetch_u128_or(&at_block, "RevealPeriodEpochs", keys, 1)
        )?;
        Ok((tempo as u64, reveal_period as u64, at_block.block_number()))
    }

    pub fn generate_timelocked_commit(
        &self,
        tempo: u64,
        reveal_period: u64,
        current_block: u64,
        hotkey_bytes: Vec<u8>,
        uids: Vec<u16>,
        values: Vec<u16>,
        version_key: u64,
    ) -> Result<(Vec<u8>, u64)> {
        bittensor_drand::generate_commit(
            uids,
            values,
            version_key,
            tempo,
            current_block,
            self.netuid,
            reveal_period,
            BLOCK_TIME,
            hotkey_bytes,
        )
        .map_err(|(_, msg)| anyhow::anyhow!("tlock encryption failed: {msg}"))
    }

    pub async fn commit_timelocked_weights(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        wallet: &Arc<Wallet>,
        commit_bytes: Vec<u8>,
        reveal_round: u64,
    ) -> Result<()> {
        let tx = subxt::dynamic::tx(
            PALLET,
            "commit_timelocked_weights",
            vec![
                Value::from(self.netuid as u64),
                Value::from_bytes(&commit_bytes),
                Value::from(reveal_round),
                Value::from(COMMIT_REVEAL_VERSION),
            ],
        );

        let signer = SubxtSr25519Signer::new(wallet)?;

        let mut tx_client = client.tx().await.context("acquiring transactions client")?;
        let progress = tokio::time::timeout(
            TX_SUBMIT_TIMEOUT,
            tx_client.sign_and_submit_then_watch_default(&tx, &signer),
        )
        .await
        .context("commit_timelocked_weights submit timed out")?
        .context("submitting commit_timelocked_weights")?;

        let result = tokio::time::timeout(
            TX_FINALIZATION_TIMEOUT,
            progress.wait_for_finalized_success(),
        )
        .await
        .context("commit_timelocked_weights finalization timed out")?
        .context("commit_timelocked_weights finalization")?;

        info!(
            hash = %result.extrinsic_hash(),
            reveal_round = reveal_round,
            "timelocked weights committed"
        );
        Ok(())
    }

    pub async fn blocks_since_last_update(
        &self,
        client: &OnlineClient<PolkadotConfig>,
        uid: u16,
    ) -> Result<u64> {
        let at_block = at_current_block(client).await?;
        let updates: Vec<u64> =
            fetch_typed::<Vec<u64>>(&at_block, "LastUpdate", netuid_keys(self.netuid))
                .await
                .context("decoding LastUpdate vec")?
                .unwrap_or_default();
        let last_update = updates.get(uid as usize).copied().unwrap_or(0);
        Ok(at_block.block_number().saturating_sub(last_update))
    }
}

pub(crate) struct SubxtSr25519Signer {
    wallet: Arc<Wallet>,
    account_id: subxt::utils::AccountId32,
}

impl SubxtSr25519Signer {
    pub(crate) fn new(wallet: &Arc<Wallet>) -> Result<Self> {
        let account_id = wallet.hotkey_account_id()?;
        let test_sig = wallet
            .sign_hotkey(b"signer_validation")
            .context("hotkey cannot produce signatures")?;
        anyhow::ensure!(
            test_sig.len() == 64,
            "hotkey signature length {} != 64",
            test_sig.len()
        );
        Ok(Self {
            wallet: Arc::clone(wallet),
            account_id,
        })
    }
}

impl Signer<PolkadotConfig> for SubxtSr25519Signer {
    fn account_id(&self) -> subxt::utils::AccountId32 {
        self.account_id
    }

    fn sign(&self, payload: &[u8]) -> <PolkadotConfig as subxt::Config>::Signature {
        let sig = match self.wallet.sign_hotkey(payload) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "hotkey signing failed in Signer trait");
                return subxt::utils::MultiSignature::Sr25519([0u8; 64]);
            }
        };
        let sig_arr: [u8; 64] = match sig.try_into() {
            Ok(arr) => arr,
            Err(v) => {
                tracing::error!(len = v.len(), "unexpected signature length from hotkey");
                return subxt::utils::MultiSignature::Sr25519([0u8; 64]);
            }
        };
        subxt::utils::MultiSignature::Sr25519(sig_arr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_timelocked_commit_produces_ciphertext() {
        let setter = WeightsSetter::new(2);
        let (ct, round) = setter
            .generate_timelocked_commit(
                360,
                1,
                1000,
                vec![1u8; 32],
                vec![1, 2, 3],
                vec![100, 200, 300],
                11003,
            )
            .expect("tlock encryption should succeed");
        assert!(!ct.is_empty());
        assert!(round > 0);
    }

    #[test]
    fn generate_timelocked_commit_different_inputs_differ() {
        let setter = WeightsSetter::new(2);
        let (ct1, _) = setter
            .generate_timelocked_commit(
                360,
                1,
                1000,
                vec![1u8; 32],
                vec![1, 2],
                vec![100, 200],
                11003,
            )
            .unwrap();
        let (ct2, _) = setter
            .generate_timelocked_commit(
                360,
                1,
                1000,
                vec![1u8; 32],
                vec![1, 3],
                vec![100, 200],
                11003,
            )
            .unwrap();
        assert_ne!(ct1, ct2);
    }

    #[test]
    fn generate_timelocked_commit_different_netuids_differ() {
        let setter1 = WeightsSetter::new(1);
        let setter2 = WeightsSetter::new(2);
        let args = (
            360u64,
            1u64,
            1000u64,
            vec![1u8; 32],
            vec![1u16, 2],
            vec![100u16, 200],
            11003u64,
        );
        let (_, round1) = setter1
            .generate_timelocked_commit(
                args.0,
                args.1,
                args.2,
                args.3.clone(),
                args.4.clone(),
                args.5.clone(),
                args.6,
            )
            .unwrap();
        let (_, round2) = setter2
            .generate_timelocked_commit(args.0, args.1, args.2, args.3, args.4, args.5, args.6)
            .unwrap();
        assert_ne!(round1, round2);
    }

    #[test]
    fn generate_timelocked_commit_reveal_round_increases_with_reveal_period() {
        let setter = WeightsSetter::new(2);
        let (_, round_1epoch) = setter
            .generate_timelocked_commit(360, 1, 1000, vec![1u8; 32], vec![1], vec![100], 11003)
            .unwrap();
        let (_, round_3epochs) = setter
            .generate_timelocked_commit(360, 3, 1000, vec![1u8; 32], vec![1], vec![100], 11003)
            .unwrap();
        assert!(
            round_3epochs > round_1epoch,
            "longer reveal period should produce later drand round"
        );
    }

    #[test]
    fn generate_timelocked_commit_empty_weights() {
        let setter = WeightsSetter::new(2);
        let (ct, round) = setter
            .generate_timelocked_commit(360, 1, 1000, vec![1u8; 32], vec![], vec![], 11003)
            .unwrap();
        assert!(!ct.is_empty());
        assert!(round > 0);
    }

    #[test]
    fn generate_timelocked_commit_large_uid_set() {
        let setter = WeightsSetter::new(2);
        let uids: Vec<u16> = (0..256).collect();
        let values: Vec<u16> = (0..256).map(|i| (i * 100) as u16).collect();
        let (ct, round) = setter
            .generate_timelocked_commit(360, 1, 50000, vec![2u8; 32], uids, values, 11003)
            .unwrap();
        assert!(!ct.is_empty());
        assert!(round > 0);
    }
}
