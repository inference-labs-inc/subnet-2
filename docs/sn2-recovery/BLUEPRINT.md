# BLUEPRINT  Dominating Miner on Any Subnet (Docs-Only)

## 1) Recon & Rules
- Confirm admission model (stake vs PoW vs allowlist).
- Read the subnets README/spec and validator expectations.
- **See:** docs/sn2-recovery/DECISIONS.txt for SN2 constraints (image tag format, monitoring policy, backups).

## 2) Wallets & Security
- Keep wallets on the server only; never commit keys.
- **Before creation set:** umask 0077 (restrictive defaults).
- Enforce: chmod -R go-rwx /root/.bittensor/wallets (or your wallet path).
- Store *addresses-only* snapshot (SS58) in a private note (no secrets).

## 3) Infra Baseline
- Host networking, stable public IP, open Axon port, synced time.
- Pin container digest; verify mounts and /dev/shm exist.

## 4) Image & Axon
- Pin GHCR digest (no floating :latest in production).
- Bind Axon on a reserved port; confirm listener locally and via public IP (curl HEAD).

## 5) Admission (Timing Strategy)
- Register only when infra is green. On stake-based nets (e.g., SN2), stake *after* registration and **only** when youre ready to answer immediately.

## 6) Performance Guardrails
- Start with --axon.max_workers 1; scale cautiously.
- CPU pinning and SHM sized appropriately; pre-sync models to NVMe.

## 7) Trust & Success Rate
- Target **99.9%** success (timeouts/rejects capped).
- Keep response latency within validator budgets; avoid cold starts.

## 8) Monitoring (Examples)
Tail logs for: Request by, Running proof, , timeout.

**Example lines to spot:**
- [INFO] Request by validator-xyz in 45ms 
- [WARN] Running proof took 89ms (budget: 60ms) timeout
- Miner Status: healthy; proofs=123; success_rate=99.95%

## 9) Rollback & Hygiene
- Keep a known-good digest and one-liner to restore.
- Keep wallets backed up (encrypted) with retention policy (30d).

