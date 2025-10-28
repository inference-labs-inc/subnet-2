# SN2 Dominating Miner  Recovery & Ops (Docs Only)
**This PR is documentation-only.** Deploy/ops scripts (e.g. deploy_super_miner.sh, pin_image.sh, rollback.sh),
Makefile targets, and scripts/acceptance.sh live in the **main miner PR**.
Do **not** run deploy steps until those files are present.

## Checklist
- [ ] Pin CI-built image tag: `ghcr.io/<org>/omron:omron-znver2-YYYYMMDD-abcdef0`
- [ ] Attach acceptance + loadtest logs
- [ ] Keep monitoring non-public (VPN/UFW allowlist only)

## Notes
- Wallet values stay only in `/opt/omron-super/.env.super` on the server; never commit them.


