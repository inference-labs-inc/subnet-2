# Versioning and Auto-Update

## Semantic Versioning

This project uses [Semantic Versioning 2.0.0](https://semver.org/). Version numbers take the form `MAJOR.MINOR.PATCH`:

1. **MAJOR** — incompatible API changes
2. **MINOR** — backwards-compatible new functionality
3. **PATCH** — backwards-compatible fixes

The workspace version is defined in the root `Cargo.toml` under `[workspace.package].version` and shared across all crates.

## Auto-Update

Both `sn2-miner` and `sn2-validator` include a built-in auto-update mechanism (`sn2_chain::auto_update`). On startup (unless `--no-auto-update` is passed), a background task:

1. Polls the [GitHub releases page](https://github.com/inference-labs-inc/subnet-2/releases) every 5 minutes
2. Compares the latest release tag against the compiled-in version
3. Downloads the matching platform binary (e.g. `sn2-miner-linux-x86_64`, `sn2-validator-macos-aarch64`) and the `SHA256SUMS` file from the release assets
4. Verifies the SHA256 checksum of the downloaded binary
5. Stages the download into the running executable's directory and atomically replaces it via rename (same filesystem, no cross-device issues)
6. Exits with code 0 so PM2 restarts with the new version

All errors during update are logged as warnings — the running binary is never interrupted on failure.

### Disabling auto-update

```console
sn2-miner --no-auto-update ...
sn2-validator --no-auto-update ...
```

## Release Process

Releases are triggered by pushing a semver tag (e.g. `0.2.0`) to the repository. The [release workflow](../.github/workflows/release.yml) builds binaries for `linux-x86_64` and `macos-aarch64`, generates a `SHA256SUMS` file, and creates a GitHub Release with all assets attached.

## Version History

See the [releases page](https://github.com/inference-labs-inc/subnet-2/releases) for a full changelog.
