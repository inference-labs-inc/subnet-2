<div align="center">

# Running on Mainnet

</div>

## Setup

Please find relevant setup documentation over in the [`shared_setup_steps.md`] file. These steps will prepare the miner and validator for use in the following steps.

## Mining

Run the following command to start a miner on mainnet

```console
cd neurons
pm2 start miner.py --name miner --interpreter ../.venv/bin/python -- \
--netuid 2 \
--wallet.name {your_miner_key_name} \
--wallet.hotkey {your_miner_hotkey_name}
```

Or run this command with `make pm2-miner WALLET_NAME={your_miner_key_name} HOTKEY_NAME={your_miner_hotkey_name}`

[View all acceptable CLI arguments →]

## Validating

Run the following command to start a validator on mainnet

```console
cd neurons
pm2 start validator.py --name validator --interpreter ../.venv/bin/python -- \
--netuid 2 \
--wallet.name {your_validator_key_name} \
--wallet.hotkey {your_validator_hotkey_name}
```

Or run this command with `make pm2-validator WALLET_NAME={validator_key_name} HOTKEY_NAME={validator_hot_key_name}`

[View all acceptable CLI arguments →]

[View all acceptable CLI arguments →]: ./command_line_arguments.md
[`shared_setup_steps.md`]: ./shared_setup_steps.md
