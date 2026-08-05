use anyhow::{Context, Result};

pub struct Wallet {
    hotkey: bittensor_wallet::Keypair,
    hotkey_ss58: String,
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

        let hotkey_ss58 = hotkey
            .ss58_address()
            .context("no ss58 address for hotkey")?;

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
            name: name.to_string(),
            hotkey_name: hotkey_name.to_string(),
            wallet_path: resolved_path,
        })
    }

    pub fn hotkey_ss58(&self) -> &str {
        &self.hotkey_ss58
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

    pub fn hotkey_account_id(&self) -> Result<subxt::utils::AccountId32> {
        let bytes = self.hotkey_public_bytes()?;
        Ok(subxt::utils::AccountId32::from(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const MNEMONIC: &str = "bottom drive obey lake curtain smoke basket hold race lonely fit walk";
    const EXPECTED_SS58: &str = "5DfhGyQdFobKM8NsWvEeAKk5EQQgYe9AydgJ7rMB6E1EqRzV";

    fn write_wallet(dir: &std::path::Path, keyfile: &str) {
        let hotkeys = dir.join("sn2-test").join("hotkeys");
        std::fs::create_dir_all(&hotkeys).expect("create hotkeys dir");
        let mut f = std::fs::File::create(hotkeys.join("default")).expect("create hotkey file");
        f.write_all(keyfile.as_bytes()).expect("write hotkey file");
    }

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sn2-wallet-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn loads_hotkey_written_without_crypto_type() {
        let dir = scratch_dir("no-crypto-type");
        write_wallet(&dir, &format!(r#"{{"secretPhrase":"{MNEMONIC}"}}"#));

        let wallet = Wallet::from_paths("sn2-test", "default", Some(dir.to_str().unwrap()))
            .expect("hotkey without cryptoType should load");
        assert_eq!(wallet.hotkey_ss58(), EXPECTED_SS58);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_hotkey_written_with_integer_crypto_type() {
        let dir = scratch_dir("crypto-type");
        write_wallet(
            &dir,
            &format!(r#"{{"secretPhrase":"{MNEMONIC}","cryptoType":1}}"#),
        );

        let wallet = Wallet::from_paths("sn2-test", "default", Some(dir.to_str().unwrap()))
            .expect("hotkey carrying an integer cryptoType should load");
        assert_eq!(wallet.hotkey_ss58(), EXPECTED_SS58);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
