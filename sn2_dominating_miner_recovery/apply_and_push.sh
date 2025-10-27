#!/usr/bin/env bash
# sn2_dominating_miner_recovery/apply_and_push.sh
# Apply the SN2 Dominating Miner mbox and push branch sn2/dominating-miner.
# - Idempotent branch creation (-B)
# - Prompts for your fork URL if needed
# - Validates mbox presence, committer identity, and clean working tree
# - Best-effort chmod on scripts/tools
# - Warns if expected dirs missing after apply
# - Writes mbox SHA256 (if tool available)
# - Pushes branch and prints PR instructions (guards PR_BODY.md)

set -euo pipefail

say() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
die() { echo "ERROR: $*" >&2; exit 2; }

# --- Resolve paths relative to this script ---
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

# --- Config (you can override via env) ---
: "${BRANCH:=sn2/dominating-miner}"
: "${REPO_DIR:=${ROOT_DIR}/omron-subnet}"

# Where the mbox is expected (next to this script by default).
# Change if you placed it elsewhere.
: "${MBOX_PATH:=${ROOT_DIR}/sn2-dominating-miner-final.mbox}"

# If REPO_URL not provided, try to detect an existing repo; otherwise prompt for your fork
: "${REPO_URL:=}"

say "Starting apply_and_push for branch: ${BRANCH}"

# --- Prereqs ---
command -v git >/dev/null 2>&1 || die "git is not installed. Install Git (Git for Windows or CLI) and re-run."

# --- Find/confirm the mbox ---
if [ ! -f "${MBOX_PATH}" ]; then
  cat >&2 <<EOF
ERROR: mbox not found at:
  ${MBOX_PATH}

Place Copilot's mbox file there (or set MBOX_PATH), then re-run:
  MBOX_PATH="/full/path/to/sn2-dominating-miner-final.mbox" bash ${SCRIPT_DIR##*/}/apply_and_push.sh
EOF
  exit 2
fi
say "Found mbox: ${MBOX_PATH}"

# --- Choose repo location / clone or reuse ---
if [ -d "${REPO_DIR}/.git" ]; then
  say "Using existing repo: ${REPO_DIR}"
else
  # If no REPO_URL provided, prompt for the user's fork URL
  if [ -z "${REPO_URL}" ]; then
    say "No REPO_URL provided and repo not present."
    echo
    echo "Paste your FORK HTTPS URL (recommended), e.g.:"
    echo "  https://github.com/<your-user>/omron-subnet.git"
    echo
    read -r -p "Fork URL: " REPO_URL
    [ -z "${REPO_URL}" ] && die "No URL provided."
  fi
  say "Cloning ${REPO_URL} -> ${REPO_DIR}"
  git clone "${REPO_URL}" "${REPO_DIR}"
fi

cd "${REPO_DIR}"

# --- Ensure committer identity set (repo-local) ---
name="$(git config user.name || true)"
email="$(git config user.email || true)"
if [ -z "${name}" ] || [ -z "${email}" ]; then
  echo
  echo "Configure your committer identity for THIS repository."
  [ -z "${name}" ]  && read -r -p "Git user.name  (e.g. stevenkita): " name
  [ -z "${email}" ] && read -r -p "Git user.email (e.g. stevenkita@users.noreply.github.com): " email
  git config user.name  "${name}"
  git config user.email "${email}"
  say "Set user.name='${name}', user.email='${email}' for this repo."
fi

# --- Ensure clean working tree ---
if [ -n "$(git status --porcelain)" ]; then
  die "Working tree is not clean in ${REPO_DIR}. Stash/commit changes and re-run."
fi

# --- Base branch refresh & idempotent feature branch ---
# We try 'origin' first. If 'origin' is your fork, it still has 'main'.
git fetch origin || true
BASE_REF="origin/main"
if ! git rev-parse --verify "${BASE_REF}" >/dev/null 2>&1; then
  # Fallback to 'upstream' remote if present
  if git remote get-url upstream >/dev/null 2>&1; then
    git fetch upstream
    BASE_REF="upstream/main"
  else
    die "Cannot find ${BASE_REF} or upstream/main. Add an upstream remote or fetch main."
  fi
fi

say "Checking out ${BRANCH} from ${BASE_REF} (idempotent)"
git checkout -B "${BRANCH}" "${BASE_REF}"

# --- Apply the mbox (with 3-way fallback) ---
say "Applying mbox with git am"
if ! git am "${MBOX_PATH}"; then
  say "git am failed; aborting and retrying with --3way…"
  git am --abort || true
  if ! git am --3way "${MBOX_PATH}"; then
    say "git am --3way failed; aborting sequence."
    git am --abort || true
    cat >&2 <<'EOF'
ERROR: Could not apply the mbox cleanly.

Hints:
  - Ensure branch is reset to a clean state from main.
  - Try manual resolve: extract patches and apply one by one.
  - Or re-generate an updated mbox against the current main.

EOF
    exit 3
  fi
fi
say "mbox applied successfully."

# --- Warn if expected dirs are missing (informational) ---
missing=0
for d in scripts tools healthcheck; do
  if [ ! -d "$d" ]; then
    echo "WARN: expected directory '$d/' not found after apply." >&2
    missing=1
  fi
done
[ "${missing}" -eq 1 ] && echo "Note: If this is intentional for your series, you can ignore the warnings." >&2

# --- Best-effort exec bits (only if files exist) ---
say "Setting executable bits where applicable (best-effort)"
[ -f deploy_super_miner.sh ]         && chmod +x deploy_super_miner.sh || true
[ -f grub_isolation.sh ]             && chmod +x grub_isolation.sh     || true
[ -f healthcheck/healthcheck.sh ]    && chmod +x healthcheck/healthcheck.sh || true

if [ -d scripts ]; then
  find scripts -maxdepth 1 -type f -name '*.sh' -exec chmod +x {} \; || true
  if [ -d scripts/systemd ]; then
    find scripts/systemd -maxdepth 1 -type f -name '*.service' -exec chmod +x {} \; || true
    find scripts/systemd -maxdepth 1 -type f -name '*.timer'   -exec chmod +x {} \; || true
  fi
fi

[ -d tools ] && find tools -maxdepth 1 -type f -name '*.py' -exec chmod +x {} \; || true

# Record mode changes if any
git add -A
git commit -m "chore: ensure exec bits for scripts/tools" || true

# --- Write SHA256 for the mbox (best-effort) ---
say "Computing mbox SHA256 (best-effort)"
(
  set -e
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "${MBOX_PATH}" | tee "${MBOX_PATH}.sha256" >/dev/null
  elif command -v shasum >/dev-null 2>&1; then
    shasum -a 256 "${MBOX_PATH}" | tee "${MBOX_PATH}.sha256" >/dev/null
  else
    echo "WARN: No sha256 tool found; skipping checksum." >&2
  fi
) || true

# --- Push to your fork (origin). If denied, guide user. ---
say "Pushing ${BRANCH} to 'origin'"
if ! git push -u origin "${BRANCH}"; then
  cat >&2 <<EOF

ERROR: Push to 'origin' failed (likely permissions).
Make sure 'origin' is YOUR FORK, not the upstream.

To fix:
  git remote set-url origin https://github.com/<your-user>/omron-subnet.git
  git push -u origin ${BRANCH}

EOF
  exit 4
fi

echo
echo "Branch pushed: ${BRANCH}"
echo "Open a PR against upstream 'main' with the provided PR body."

# --- Print PR creation guidance ---
if [ -f PR_BODY.md ]; then
  cat <<'EOF'
If you use GitHub CLI:
  gh pr create --title "SN2 Dominating Miner: >0.99 trust, znver2 build, watchdog, metrics, loadtest & CI" \
               --body-file PR_BODY.md --base main --head sn2/dominating-miner
EOF
else
  cat <<'EOF'
NOTE: PR_BODY.md is not present in the repo root.
Open the compare URL in your browser and paste the PR body you saved from Copilot.
EOF
fi

# Try to render a handy compare URL (fork -> upstream)
origin_url="$(git config --get remote.origin.url || true)"
if printf '%s\n' "${origin_url}" | grep -qi '^https://github.com/'; then
  fork_spec="$(printf '%s\n' "${origin_url}" | sed -E 's#^https://github.com/([^/]+/[^.]+)(\.git)?$#\1#')"
  upstream_repo="https://github.com/inference-labs-inc/omron-subnet"
  echo
  echo "Open PR here:"
  echo "  ${upstream_repo}/compare/main...${fork_spec}:${BRANCH}?expand=1"
fi

say "Done."
