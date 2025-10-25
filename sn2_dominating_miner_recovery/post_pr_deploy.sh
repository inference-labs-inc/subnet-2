#!/usr/bin/env bash
# Post-PR deploy helper for the target server
set -euo pipefail

PROJECT_DIR="/opt/omron-super"

echo "[1/4] Ensure env file exists"
if [ ! -f "${PROJECT_DIR}/.env.super" ]; then
  cp "${PROJECT_DIR}/.env.super.template" "${PROJECT_DIR}/.env.super"
  echo "Edit ${PROJECT_DIR}/.env.super and set WALLET_PATH, WALLET_NAME, HOTKEY."
fi

echo "[2/4] Deploy stack"
sudo bash "${PROJECT_DIR}/deploy_super_miner.sh" "${PROJECT_DIR}"

echo "[3/4] Enable watchdog"
sudo make -C "${PROJECT_DIR}" install-watchdog

echo "[4/4] Acceptance & loadtest"
bash "${PROJECT_DIR}/scripts/acceptance.sh" "${PROJECT_DIR}"
make -C "${PROJECT_DIR}" loadtest || true

echo "Done."
