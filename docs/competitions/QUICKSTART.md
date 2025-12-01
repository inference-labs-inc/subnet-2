# Competition Quick Start Guide

## Prerequisites

- Python 3.8+
- Storage provider credentials (R2/S3)
- Registration on subnet 2 (use `btcli register` if not already registered)
- At least 16GB RAM (32GB recommended)
- 50GB free SSD storage
- Apple Silicon (M1/M2/M3) CPU

> [!NOTE]
> While miners can run on any platform, macOS arm64 with Metal acceleration is the recommended configuration since validators use this architecture for evaluation. This helps ensure your circuit will perform consistently when being evaluated.

> [!IMPORTANT]
> Ensure your system meets the minimum resource requirements before proceeding. Circuit compilation and proving can be resource-intensive, especially during initial setup.

## Getting Started

### Configure Storage

Create a `.env` file in your project root with the following variables:

```bash
# Required - choose either R2 or S3
STORAGE_PROVIDER="r2"  # or "s3"
STORAGE_BUCKET="your-bucket"

# For Cloudflare R2
STORAGE_ACCOUNT_ID="your-account-id"
STORAGE_ACCESS_KEY="your-access-key"
STORAGE_SECRET_KEY="your-secret-key"
STORAGE_REGION="auto"

# For AWS S3
# STORAGE_REGION="your-aws-region"
```

### Start the Miner

> [!IMPORTANT]
>
> **Port `8091` must be open** for your circuit to function properly. This requires configuration on your local machine and router, or, if you're using a cloud provider, adjustments to your network settings. Validators cannot query your circuit if port `8091` remains closed.

```bash
pm2 start neurons/miner.py --name subnet_2_miner -- --netuid 2 --wallet.name your_wallet --logging.debug
```

## Circuit Submission Flow

### Prepare Circuit Files

Required files in your circuit directory:

- `settings.json` - Circuit configuration with required settings:
  ```json
  {
    "run_args": {
      "input_visibility": "Private",
      "output_visibility": "Public",
      "param_visibility": "Private",
      "commitment": "KZG"
    }
  }
  ```
- `model.compiled` - Compiled model file

For an example of how to compile this circuit, see the following.

> [!NOTE]
> This script generates it's own model and is for demonstration purposes only. To compile your own model based on the competition template ONNX, please find it within `neurons/_validator/competitions/1/age.onnx` along with an example `input.json` file.

```bash
./neurons/scripts/create_competition_circuit.py
```

### Deploy Circuit

Place circuit files in the `./competition_circuit/` directory:

```bash
mkdir -p competition_circuit/
cp -r your_circuit/* competition_circuit/
```

The miner will automatically:

- Monitor circuit directory for changes
- Upload modified files to R2/S3
- Create on-chain commitment using the SHA256 hash of the `model.compiled` file
- Generate time-limited signed URLs for validators upon request

### Monitor Evaluation

- Watch validator requests: `pm2 logs miner`
- View metrics: https://wandb.ai/inferencelabs/sn2
- View leaderboard: https://sn2-accelerate.inferencelabs.com

### 🏆 EZKL Performance Evaluation

Upon completion of the Subnet 2 competition phase, [EZKL](https://ezkl.xyz) will conduct an independent evaluation of all submitted circuits through their automated CI/CD pipeline. Your implementation will be assessed across three critical dimensions:

- ⚡ **Performance** - Circuit proving time optimization
- 📊 **Resource Efficiency** - Memory utilization and management
- 🎯 **Accuracy** - Age recognition precision and reliability

#### Submission Process

1. Submit your implementation via PR to https://github.com/zkonduit/ezkl
2. Include the tag `subnet-2-competition-1`
3. Await automated evaluation results

High-performing circuits that demonstrate excellence across these metrics will be eligible for additional grant funding. EZKL's evaluation criteria are designed to identify implementations that achieve an optimal balance of performance, efficiency, and accuracy.

## Troubleshooting

**Circuit Upload Fails**

- Verify storage credentials in .env
- Check network connectivity
- Verify all required files are present
- Check storage bucket permissions

**Validation Errors**

- Review validator logs for specific failures via WandB
- Verify input schema matches implementation

## FAQ

### How do I benchmark my circuit?

To benchmark your circuit before submission to mainnet, it's recommended to run a validator and miner locally, pointed towards testnet or a local network. For guidance on how to do this, please refer to [testnet] and [localnet] guides respectively.

### Which hardware do validators run to benchmark my circuit?

Validators are instructed to run using macOS arm64 architectures. M1 or M2 processors are recommended for best performance and the majority of validators will be using these processors to test your circuits.
For specific hardware requirements, please refer to the main [README] document.

### What is the maximum proof time allowed for a circuit?

The maximum proof time is a configurable property and is subject to change over time, however as it stands this value is set at 300 seconds (5 minutes).

### Do I need to upload `vk.key`/`pk.key`?

No. Only `model.compiled` and `settings.json` are required. During evaluation, the validator generates `vk.key` and `pk.key` locally using `ezkl setup`. This setup step is limited to exactly 5 minutes (timeout) and capped at 16 GiB of address space. If it fails or times out, the submission receives no score for that evaluation. See the canonical implementation of these limits in `neurons/_validator/competitions/services/circuit_evaluator.py` within the `_ensure_keys` function (the `set_limits` inner function and the `timeout=FIVE_MINUTES` argument).

### When does the competition end?

The competition will end on 2025-04-27.

## For additional assistance

- Join Discord and reach out via the Subnet 2 channel: https://discord.gg/bittensor
- Otherwise, feel free to open a GitHub issue within the repository.

[testnet]: ../running_on_testnet.md
[localnet]: ../running_on_staging.md
[README]: ../../README.md#minimum-1
