#!/usr/bin/env bash
# Apply Copilot mbox and push sn2/dominating-miner
set -euo pipefail

REPO_URL="https://github.com/inference-labs-inc/omron-subnet.git"
REPO_DIR="${PWD}/omron-subnet"
BRANCH="sn2/dominating-miner"
MBOX_PATH="${PWD}/sn2-dominating-miner-final.mbox"

if [ ! -f "${MBOX_PATH}" ]; then
  echo "ERROR: mbox not found at ${MBOX_PATH}"
  echo "Place the file here (same dir) and re-run."
  exit 2
fi

if [ ! -d "${REPO_DIR}" ]; then
  git clone "${REPO_URL}" "${REPO_DIR}"
fi

cd "${REPO_DIR}"
git fetch origin
git checkout -b "${BRANCH}" origin/main

# ensure clean
if [ -n "$(git status --porcelain)" ]; then
  echo "Working tree not clean; aborting."
  exit 3
fi

git am "${MBOX_PATH}"

# ensure exec bits (some setups drop modes)
chmod +x scripts/*.sh scripts/systemd/*.service scripts/systemd/*.timer tools/*.py 2>/dev/null || true
git add -A
git commit -m "chore: ensure exec bits for scripts/tools" || true

# optional audit checksum
( cd - >/dev/null 2>&1 && sha256sum "sn2-dominating-miner-final.mbox" | tee "sn2-dominating-miner-final.mbox.sha256" ) || true

git push -u origin "${BRANCH}"

echo
echo "Branch pushed: ${BRANCH}"
echo "Open a PR to main with the provided PR body."
echo "If using GitHub CLI:"
echo '  gh pr create --title "SN2 Dominating Miner: >0.99 trust, znver2 build, watchdog, metrics, loadtest & CI" \'
echo '                 --body-file PR_BODY.md --base main --head sn2/dominating-miner'
