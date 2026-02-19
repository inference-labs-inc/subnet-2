use anyhow::{Context, Result};

pub struct Wallet {
    hotkey: bittensor_wallet::Keypair,
    hotkey_ss58: String,
    coldkey_ss58: String,
    pub name: String,
    pub hotkey_name: String,
    pub wallet_path: String,
}

impl Wallet {
    pub fn from_paths(name: &str, hotkey_name: &str, wallet_path: Option<&str>) -> Result<Self> {
        let bt_wallet = bittensor_wallet::Wallet::new(
            Some(name.to_string()),
            Some(hotkey_name.to_string()),
            wallet_path.map(|p| p.to_string()),
            None,
        );

        let hotkey = bt_wallet
            .get_hotkey(None)
            .map_err(|e| anyhow::anyhow!("loading hotkey: {:?}", e))?;

        let coldkeypub = bt_wallet
            .get_coldkeypub(None)
            .map_err(|e| anyhow::anyhow!("loading coldkeypub: {:?}", e))?;

        let hotkey_ss58 = hotkey
            .ss58_address()
            .context("no ss58 address for hotkey")?;

        let coldkey_ss58 = coldkeypub
            .ss58_address()
            .context("no ss58 address for coldkeypub")?;

        let resolved_path = match wallet_path {
            Some(p) => p.to_string(),
            None => {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
                format!("{home}/.bittensor/wallets")
            }
        };

        Ok(Wallet {
            hotkey,
            hotkey_ss58,
            coldkey_ss58,
            name: name.to_string(),
            hotkey_name: hotkey_name.to_string(),
            wallet_path: resolved_path,
        })
    }

    pub fn hotkey_ss58(&self) -> &str {
        &self.hotkey_ss58
    }

    pub fn coldkey_ss58(&self) -> &str {
        &self.coldkey_ss58
    }

    pub fn sign_hotkey(&self, data: &[u8]) -> Result<Vec<u8>> {
        self.hotkey
            .sign(data.to_vec())
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    pub fn hotkey_public_bytes(&self) -> Result<[u8; 32]> {
        let bytes = self
            .hotkey
            .public_key()
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .context("no public key")?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("public key not 32 bytes"))?;
        Ok(arr)
    }

    pub(crate) fn hotkey_account_id(&self) -> Result<subxt::utils::AccountId32> {
        let bytes = self.hotkey_public_bytes()?;
        Ok(subxt::utils::AccountId32::from(bytes))
    }
}
