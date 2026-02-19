## Running Your Own Subtensor Chain Locally

This tutorial guides you through setting up a local subtensor chain, creating a subnetwork, and running subnet-2 against it.

### 1. Install substrate dependencies

```bash
sudo apt update
sudo apt install --assume-yes make build-essential git clang curl libssl-dev llvm libudev-dev protobuf-compiler
```

### 2. Install Rust and Cargo

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### 3. Clone the Subtensor Repository

```bash
git clone https://github.com/opentensor/subtensor.git
```

### 4. Switch to the Devnet-Ready Branch

```bash
cd subtensor
git fetch origin subnets/devnet-ready
git checkout subnets/devnet-ready
```

### 5. Setup Rust for Substrate Development

```bash
./subtensor/scripts/init.sh
```

### 6. Initialize Your Local Subtensor Chain

```bash
./scripts/localnet.sh
```

> [!NOTE]
> If building for the first time, this step will take a while depending on your hardware.

### 7. Set Up Wallets

```bash
btcli wallet new_coldkey --wallet.name owner
btcli wallet new_coldkey --wallet.name miner
btcli wallet new_hotkey --wallet.name miner --wallet.hotkey default
btcli wallet new_coldkey --wallet.name validator
btcli wallet new_hotkey --wallet.name validator --wallet.hotkey default
```

### 8. Mint tokens

```bash
btcli wallet faucet --wallet.name owner --subtensor.chain_endpoint ws://127.0.0.1:9944
btcli wallet faucet --wallet.name validator --subtensor.chain_endpoint ws://127.0.0.1:9944
```

### 9. Create a Subnetwork

```bash
btcli subnet create --wallet.name owner --subtensor.chain_endpoint ws://127.0.0.1:9944
```

### 10. Register Validator and Miner Keys

```bash
btcli subnet register --wallet.name miner --wallet.hotkey default --subtensor.chain_endpoint ws://127.0.0.1:9944
btcli subnet register --wallet.name validator --wallet.hotkey default --subtensor.chain_endpoint ws://127.0.0.1:9944
```

### 11. Stake to your validator

```bash
btcli stake add --wallet.name validator --wallet.hotkey default --subtensor.chain_endpoint ws://127.0.0.1:9944
```

### 12. Run Miner and Validator

```bash
pm2 start target/release/sn2-miner --name miner --kill-timeout 3000 -- \
  --netuid 1 \
  --subtensor-chain-endpoint ws://127.0.0.1:9944 \
  --wallet-name miner \
  --wallet-hotkey default

pm2 start target/release/sn2-validator --name validator --kill-timeout 3000 -- \
  --netuid 1 \
  --subtensor-chain-endpoint ws://127.0.0.1:9944 \
  --wallet-name validator \
  --wallet-hotkey default
```

### 13. Monitor

```bash
pm2 monit
```

[View all CLI arguments →](./command_line_arguments.md)
