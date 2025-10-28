# PR Body placeholder

Paste the full PR_BODY.md Copilot supplied into the PR description.
Key points:
- Optimized znver2 Dockerfile, runtime profile (read_only rootfs, tmpfs /tmp), watchdog & metrics exporter.
- CI builds GHCR tag `omron-znver2-<DATE>-<GITSHA>`; add GHCR_TOKEN in repo secrets.
- Acceptance + loadtest steps included; target success_rate >= 99.9%.
