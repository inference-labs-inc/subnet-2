<div align="center">

# Running on Testnet

</div>

## Setup

Complete the prerequisite steps in [`shared_setup_steps.md`] before proceeding.

## Mining

```console
make pm2-miner WALLET_NAME={your_miner_key_name} WALLET_HOTKEY={your_miner_hotkey_name} NETUID=118 ARGS="--network test"
```

Or directly (use `./sn2-miner` for pre-built binaries, `target/release/sn2-miner` for source builds):

```console
pm2 start ./sn2-miner --name subnet-2-miner --kill-timeout 3000 -- \
  --wallet-name {your_miner_key_name} \
  --wallet-hotkey {your_miner_hotkey_name} \
  --netuid 118 \
  --network test
```

## Validating

```console
make pm2-validator WALLET_NAME={validator_key_name} WALLET_HOTKEY={validator_hotkey_name} NETUID=118 ARGS="--network test"
```

Or directly (use `./sn2-validator` for pre-built binaries, `target/release/sn2-validator` for source builds):

```console
pm2 start ./sn2-validator --name subnet-2-validator --kill-timeout 3000 -- \
  --wallet-name {validator_key_name} \
  --wallet-hotkey {validator_hotkey_name} \
  --netuid 118 \
  --network test
```

[View all CLI arguments →](./command_line_arguments.md)

[`shared_setup_steps.md`]: ./shared_setup_steps.md
