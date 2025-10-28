# SN2 Dominating Miner — Recovery & Ops Guide
**Prereqs:** The deploy/ops scripts (e.g. `deploy_super_miner.sh`, `scripts/pin_image.sh`, `scripts/rollback.sh`), the `Makefile` targets (e.g. `loadtest`), and `scripts/acceptance.sh` are part of the main SN2 miner code PR. This PR is docs-only. Deployment/ops scripts, Makefile targets and acceptance checks live in the main SN2 miner code PR. Do not run the deployment steps here until those files exist in your working tree (after the main code PR merges or you add them).


This folder contains copy‑paste commands and scripts to:
- apply Copilot’s **mbox** series and open the PR,
- pin the optimized image tag after CI,
- deploy on the server and run acceptance/loadtest,
- rollback safely.

> You still need the actual `sn2-dominating-miner-final.mbox` text that Copilot provided.
> Save it next to these files as `sn2-dominating-miner-final.mbox` before running `apply_and_push.sh`.

## 1) Apply patches & push PR (one paste)
```bash
bash apply_and_push.sh
```

What it does:
- clones `inference-labs-inc/omron-subnet` (if not present),
- creates branch `sn2/dominating-miner`,
- applies `sn2-dominating-miner-final.mbox` with `git am`,
- fixes executable bits (if needed),
- pushes the branch.
It then prints the exact `gh pr create` command.

## 2) After PR merge (CI builds optimized image)
Add repo secret `GHCR_TOKEN` (write:packages).
After merge to `main`, pin the image tag shown in CI `release-note.txt`:
```bash
# example
scripts/pin_image.sh ghcr.io/<org>/omron:omron-znver2-YYYYMMDD-abcdef
```

## 3) Deploy on the server
Copy the repo to `/opt/omron-super`, edit `.env.super`, then:
```bash
# one-liner
sudo bash deploy_super_miner.sh /opt/omron-super && sudo make install-watchdog
# acceptance
bash /opt/omron-super/scripts/acceptance.sh /opt/omron-super
# loadtest
make loadtest
```

## 4) Rollback (if needed)
```bash
# roll back to a previous IMAGE tag
/opt/omron-super/scripts/rollback.sh ghcr.io/<org>/omron:<previous-tag> [--revert-grub]
```

---

See `DECISIONS.txt` for finalized choices. Keep `MONITORING_PUBLIC=false` unless you explicitly whitelist an admin IP via UFW.
