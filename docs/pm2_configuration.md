# PM2 Configuration

The recommended way to run miner and validator processes is via the makefile targets, which handle building and PM2 process management:

```console
make pm2-miner WALLET_NAME={name} WALLET_HOTKEY={hotkey}
make pm2-validator WALLET_NAME={name} WALLET_HOTKEY={hotkey}
```

These targets build the release binaries, remove any existing PM2 process with the same name, and start a new one with a 3-second kill timeout.

## Custom PM2 configuration

For more control, start the binaries directly with PM2. Use `target/release/sn2-miner` if you built from source, or `./sn2-miner` if you downloaded a pre-built binary:

```console
pm2 start ./sn2-miner \
  --name subnet-2-miner \
  --kill-timeout 3000 \
  -- \
  --wallet-name {name} \
  --wallet-hotkey {hotkey} \
  --netuid 2
```

Additional flags can be appended after the `--` separator. See [Command Line Arguments](./command_line_arguments.md) for the full list.

## Useful PM2 commands

| Command | Description |
|---------|-------------|
| `pm2 status` | List running processes |
| `pm2 logs subnet-2-miner` | Stream miner logs |
| `pm2 logs subnet-2-validator` | Stream validator logs |
| `pm2 monit` | Interactive process monitor |
| `pm2 stop subnet-2-miner` | Stop the miner |
| `pm2 restart subnet-2-validator` | Restart the validator |

## Auto-update interaction

The binaries include a built-in auto-update mechanism that replaces the binary on disk and exits with code 0. PM2 will automatically restart the process with the new binary. If you prefer to manage updates manually, pass `--no-auto-update` to disable this behavior.
