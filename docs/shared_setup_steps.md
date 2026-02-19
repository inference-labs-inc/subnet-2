# Setup Instructions

For miners and validators.

## 1. Install Prerequisites

### Option A: Pre-built binaries (recommended)

Download the latest release for your platform from [GitHub Releases](https://github.com/inference-labs-inc/subnet-2/releases). Binaries are available for `linux-x86_64` and `macos-aarch64`.

**Linux (x86_64):**

```console
curl -L -o sn2-miner https://github.com/inference-labs-inc/subnet-2/releases/latest/download/sn2-miner-linux-x86_64
curl -L -o sn2-validator https://github.com/inference-labs-inc/subnet-2/releases/latest/download/sn2-validator-linux-x86_64
chmod +x sn2-miner sn2-validator
```

**macOS (Apple Silicon):**

```console
curl -L -o sn2-miner https://github.com/inference-labs-inc/subnet-2/releases/latest/download/sn2-miner-macos-aarch64
curl -L -o sn2-validator https://github.com/inference-labs-inc/subnet-2/releases/latest/download/sn2-validator-macos-aarch64
chmod +x sn2-miner sn2-validator
```

You will also need:

| Tool | Description |
|------|-------------|
| [`pm2`] | Process manager for running binaries in the background |
| [`btcli`] | CLI for interacting with the Bittensor network |

### Option B: Build from source

Install the Rust toolchain and build the binaries:

```console
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

git clone https://github.com/inference-labs-inc/subnet-2.git
cd subnet-2
cargo build --release --bin sn2-validator --bin sn2-miner
```

Binaries will be at `target/release/sn2-validator` and `target/release/sn2-miner`.

### Option C: Docker

No local toolchain required. See the [Docker instructions in the README](../README.md#run-the-miner).

## 2. Create a new wallet

> [!NOTE]
> Skip this step if you already have a wallet configured in [`btcli`].

> [!WARNING]
> This step will create a new seed phrase. If lost, it will no longer be possible to access your account. Please write it down and store it in a secure location.

```console
btcli w new_coldkey
btcli w new_hotkey
```

## 3. Register on the subnet

> [!CAUTION]
> When registering on a subnet, you are required to burn ('recycle') a dynamic amount of tao. This tao will not be refunded if you are deregistered.

Replace `default` values below with your wallet and hotkey names if they are not `default`.

| Variable  | Description |
|-----------|-------------|
| `NETWORK` | `finney` for mainnet or `test` for testnet |
| `NETUID`  | `2` on mainnet, `118` on testnet |

```console
btcli subnet register --subtensor.network {NETWORK} --netuid {NETUID} --wallet.name default --wallet.hotkey default
```

## 4. Run your miner or validator

Follow the instructions for your target network:

[Mainnet "Finney" →](./running_on_mainnet.md)
[Testnet →](./running_on_testnet.md)
[Local "Staging" Network →](./running_on_staging.md)

[`pm2`]: https://pm2.keymetrics.io/docs/usage/quick-start/
[`btcli`]: https://docs.bittensor.com/getting-started/installation
