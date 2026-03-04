# Command Line Arguments

All arguments use `--long-flag` syntax. Flags are shared between miner and validator unless noted otherwise.

## Shared Arguments

| Argument | Default | Description |
|----------|---------|-------------|
| `--netuid` | `2` | The subnet UID |
| `--network` | `finney` | Network to connect to: `finney`, `test`, `local`, or a custom endpoint |
| `--subtensor-chain-endpoint` | Derived from `--network` | Override the subtensor WebSocket endpoint directly |
| `--wallet-name` | `default` | Bittensor wallet name |
| `--wallet-hotkey` | `default` | Wallet hotkey name |
| `--wallet-path` | `~/.bittensor/wallets` | Path to wallet directory |
| `--log-level` | `info` | Tracing filter directive (e.g. `debug`, `warn`, `sn2_validator=trace`) |
| `--no-auto-update` | `false` | Disable the built-in binary auto-update mechanism |

## Miner Arguments

| Argument | Default | Description |
|----------|---------|-------------|
| `--axon-host` | `0.0.0.0` | Bind address for the HTTP axon server |
| `--axon-port` | `8091` | HTTP axon port |
| `--quic-port` | `8092` | QUIC ([btlightning](https://github.com/inference-labs-inc/lightning)) server port |
| `--external-ip` | None | Public IP to register on-chain for the axon |
| `--dsperse-socket` | None | dsperse prover socket address |
| `--circuit-dir` | `competition_circuit` | Directory for competition circuit files |
| `--storage-bucket` | None | S3 bucket for circuit storage |

## Validator Arguments

| Argument | Default | Description |
|----------|---------|-------------|
| `--max-concurrency` | `32` | Maximum concurrent miner queries |
| `--api-miners-pct` | `20` | Percentage of miners allocated to API requests |
| `--disable-benchmark` | `false` | Disable benchmark queries |
| `--relay-url` | None | WebSocket relay URL |
| `--no-relay` | `false` | Disable the relay WebSocket connection (enabled by default) |
| `--metrics-port` | `9090` | Prometheus metrics exporter port |
| `--dsperse-socket` | None | dsperse prover socket address |

## Environment Variables

Tracing can be configured via the `RUST_LOG` environment variable, which takes precedence over `--log-level`. The syntax follows the [tracing-subscriber `EnvFilter` directives](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html).

```console
RUST_LOG=debug sn2-validator --netuid 2
RUST_LOG=sn2_miner=trace,sn2_chain=debug sn2-miner --netuid 2
```
