# Developer Guide

This document provides a guide for developers who contribute to subnet 2.

The miner and validator are native Rust binaries organized as a Cargo workspace; see [README.md](README.md#architecture) for the crate layout.

## Toolchain

The pinned toolchain (channel and components) is declared in [`rust-toolchain.toml`](rust-toolchain.toml) and applied automatically by `rustup` on first invocation.

Enable the repository's pre-commit hook (runs `cargo fmt --check`) once after cloning:

```sh
make setup
```

## Adding Dependencies

Add a workspace member dependency by editing the relevant crate's `Cargo.toml` under `crates/`, or run:

```sh
cargo add <crate-name> -p <workspace-member>
```

Lockfile updates land in `Cargo.lock` — commit it alongside the manifest change.

## Updating Dependencies

```sh
# update a single crate to the latest semver-compatible version
cargo update -p <crate-name>

# update everything within semver constraints
cargo update
```

For a major-version bump, edit the version in the relevant `Cargo.toml` and run `cargo update -p <crate-name>`.

## Build, Test, Lint

The `makefile` wraps the common cargo invocations:

| Target | Equivalent |
|---|---|
| `make cargo-build` | `cargo build --release --locked --bin sn2-validator --bin sn2-miner` |
| `make check` | `cargo check --workspace` |
| `make clippy` | `cargo clippy --workspace -- -D warnings` |
| `make test` | `cargo test --workspace` |
| `make fmt` / `make fmt-check` | `cargo fmt --all` / `cargo fmt --all -- --check` |

Run `make clippy` and `make fmt-check` before pushing — CI rejects warnings and unformatted code.

## Running Locally

For end-to-end local execution against mainnet, testnet, or a local subtensor, follow [`docs/shared_setup_steps.md`](docs/shared_setup_steps.md) and the network-specific guides in [`docs/`](docs/). The PM2 and Docker invocations in [README.md](README.md#run-the-miner) cover the standard runtime paths.
