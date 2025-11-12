# Developer Guide

This document provides a guide for developers who contribute to subnet 2.

## Adding Dependencies

We use `uv` to manage dependencies. To add new dependencies, follow the steps below:

1. Add the package to `pyproject.toml`:

```sh
uv add <package-name>
```

2. Lock dependencies and generate `requirements.txt`:

```sh
uv lock
uv export -o requirements.txt
```

3. Sync dependencies:

```sh
uv sync
```

## Updating Dependencies

To force uv to update all packages in an existing `pyproject.toml`, run `uv sync --upgrade`.

```sh
# only update the bittensor package
$ uv sync --upgrade-package bittensor

# update both the bittensor and requests packages
$ uv sync --upgrade-package bittensor --upgrade-package requests

# update the bittensor package to the latest, and requests to v2.0.0
$ uv sync --upgrade-package bittensor --upgrade-package requests==2.0.0
```

## Running Locally for Development

For local development, we recommend using our devcontainer which provides a pre-configured development environment. The devcontainer image is pulled from `ghcr.io/inference-labs-inc/bittensor-devcontainer:latest`.

1. Create the `~/.bittensor/subnet-2` directory on your host machine if it doesn't exist
2. Open the project in VS Code with the Dev Containers extension installed
3. VS Code will prompt you to "Reopen in Container" - click this to start the devcontainer
4. Once the container starts, run:
   ```sh
   uv sync
   ```
   This will create and activate a virtual environment in `.venv`
5. In separate terminal windows, run:

   ```sh
   # Terminal 1: Start the local subnet
   start_localnet.sh

   # Terminal 2: Start the miner
   python neurons/miner.py --localnet

   # Terminal 3: Start the validator
   python neurons/validator.py --localnet
   ```

Note: btcli is pre-configured to use `ws://127.0.0.1:9944` in `~/.bittensor/config.yml`
