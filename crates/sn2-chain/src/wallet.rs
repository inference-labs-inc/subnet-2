use anyhow::{Context, Result};
use sp_core::crypto::Ss58Codec;
use sp_core::sr25519;
use sp_core::Pair;
use std::path::PathBuf;

pub struct Wallet {
    pub hotkey: sr25519::Pair,
    pub coldkey: sr25519::Pair,
    pub hotkey_ss58: String,
    pub coldkey_ss58: String,
    pub name: String,
    pub hotkey_name: String,
}

impl Wallet {
    pub fn from_paths(name: &str, hotkey_name: &str, wallet_path: Option<&str>) -> Result<Self> {
        let base = match wallet_path {
            Some(p) => PathBuf::from(p),
            None => dirs_next::home_dir()
                .context("no home directory")?
                .join(".bittensor")
                .join("wallets"),
        };
        let wallet_dir = base.join(name);

        let hotkey_path = wallet_dir.join("hotkeys").join(hotkey_name);
        let coldkey_path = wallet_dir.join("coldkeypub.txt");

        let hotkey_data = std::fs::read_to_string(&hotkey_path)
            .with_context(|| format!("reading hotkey from {}", hotkey_path.display()))?;
        let hotkey_json: serde_json::Value =
            serde_json::from_str(&hotkey_data).context("parsing hotkey JSON")?;

        let hotkey_secret = hotkey_json
            .get("secretSeed")
            .or_else(|| hotkey_json.get("secretPhrase"))
            .and_then(|v| v.as_str())
            .context("no secretSeed or secretPhrase in hotkey file")?;

        let hotkey = if hotkey_secret.starts_with("0x") {
            let seed_bytes = hex::decode(hotkey_secret.trim_start_matches("0x"))
                .context("decoding hotkey seed hex")?;
            let seed: [u8; 32] = seed_bytes
                .try_into()
                .map_err(|_| anyhow::anyhow!("hotkey seed must be 32 bytes"))?;
            sr25519::Pair::from_seed(&seed)
        } else {
            sr25519::Pair::from_string(hotkey_secret, None)
                .map_err(|e| anyhow::anyhow!("parsing hotkey phrase: {:?}", e))?
        };

        let coldkey_data = std::fs::read_to_string(&coldkey_path)
            .with_context(|| format!("reading coldkey from {}", coldkey_path.display()))?;
        let coldkey_json: serde_json::Value =
            serde_json::from_str(&coldkey_data).context("parsing coldkey JSON")?;

        let coldkey_ss58 = coldkey_json
            .get("ss58Address")
            .and_then(|v| v.as_str())
            .context("no ss58Address in coldkey file")?
            .to_string();

        let coldkey_secret = coldkey_json
            .get("secretSeed")
            .or_else(|| coldkey_json.get("secretPhrase"))
            .and_then(|v| v.as_str());

        let coldkey = match coldkey_secret {
            Some(secret) => {
                if secret.starts_with("0x") {
                    let seed_bytes = hex::decode(secret.trim_start_matches("0x"))
                        .context("decoding coldkey seed hex")?;
                    let seed: [u8; 32] = seed_bytes
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("coldkey seed must be 32 bytes"))?;
                    sr25519::Pair::from_seed(&seed)
                } else {
                    sr25519::Pair::from_string(secret, None)
                        .map_err(|e| anyhow::anyhow!("parsing coldkey phrase: {:?}", e))?
                }
            }
            None => sr25519::Pair::from_seed(&[0u8; 32]),
        };

        let hotkey_ss58 = hotkey.public().to_ss58check();

        Ok(Wallet {
            hotkey,
            coldkey,
            hotkey_ss58,
            coldkey_ss58,
            name: name.to_string(),
            hotkey_name: hotkey_name.to_string(),
        })
    }

    pub fn hotkey_seed(&self) -> [u8; 32] {
        let mut seed = [0u8; 32];
        let pair_bytes = self.hotkey.to_raw_vec();
        seed.copy_from_slice(&pair_bytes[..32]);
        seed
    }
}
