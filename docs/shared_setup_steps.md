# Setup Instructions

For miners and validators.

## 1. Install Prerequisites

To mine and validate for the subnet, you'll need to install several prerequisite tools. For convenience, we offer a shell script to install all of the required tools automatically. To run the script, use the below command. Some dependencies will be installed automatically upon starting the miner or validator, as part of pre-flight checks. Otherwise, to manually install the necessary tools, please find links to all relevant installation documentation below.

> [!IMPORTANT]
> When starting the miner or validator, you must monitor initial startup logs. If any dependencies are missing, the script will automatically attempt to install them. It _may_ prompt you to restart your system if necessary. Once all dependencies are installed, the pre-flight checks will pass without any further action required from you.

```console
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/inference-labs-inc/subnet-2/main/setup.sh)"
```

| Tool       | Description                                                                                               |
| ---------- | --------------------------------------------------------------------------------------------------------- |
| [`NodeJS`] | A JavaScript runtime that is widely used for building web applications.                                   |
| [`pm2`]    | A process manager for Node.js applications that is used to run and manage applications in the background. |
| [`Python`] | A programming language that is widely used for scientific computing and data analysis.                    |
| [`pip`]    | A package manager for Python that is used to install and manage Python packages.                          |
| [`btcli`]  | A command-line interface for interacting with the Bittensor network.                                      |

## 2. Create a new wallet

> [!NOTE]
> Skip this step if you already have a wallet configured in [`btcli`].

> [!WARNING]
> This step will create a new seed phrase. If lost, it will no longer be possible to access your account. Please write it down and store it in a secure location.

Use the below commands to create a new coldkey and hotkey for use within the Bittensor network.

```console
btcli w new_coldkey
btcli w new_hotkey
```

## 3. Register on the subnet

Run the following command to register on the subnet. You are required to register in order to mine or validate on the subnet.

> [!CAUTION]
> When registering on a subnet, you are required to burn ('recycle') a dynamic amount of tao. This tao will not be refunded in the event that you are deregistered. After running the below command, you will be asked to confirm the value for recycle two times before registering.

Replace `default` values below with your wallet and hotkey names if they are not `default`.

| Variable  | Description                                                                                                              |
| --------- | ------------------------------------------------------------------------------------------------------------------------ |
| `NETWORK` | The network you are registering on. This can be either `finney` for mainnet or `test` for testnet.                       |
| `NETUID`  | The network ID of the subnet you are registering on. For testnet, our netuid is `118` and on mainnet, our netuid is `2`. |

```console
btcli subnet register --subtensor.network {NETWORK} --netuid {NETUID} --wallet.name default --wallet.hotkey default
```

## 4. Run your miner or validator

To run your miner or validator, follow the instructions linked below based on the network you intend to mine or validate on.

[Local "Staging" Network →](./running_on_staging.md)
[Mainnet "Finney" →](./running_on_mainnet.md)
[Testnet →](./running_on_testnet.md)

[`NodeJS`]: https://nodejs.org/en/download/
[`pm2`]: https://pm2.keymetrics.io/docs/usage/quick-start/
[`Python`]: https://www.python.org/downloads/
[`pip`]: https://pip.pypa.io/en/stable/installation/
[`btcli`]: https://docs.bittensor.com/getting-started/installation
