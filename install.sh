#!/usr/bin/env bash
set -euo pipefail

REPO="inference-labs-inc/subnet-2"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
NETWORK="$(echo "${NETWORK:-mainnet}" | tr '[:upper:]' '[:lower:]')"
BINARY="${1:-all}"
OIDC_ISSUER="https://token.actions.githubusercontent.com"
SIGNER_IDENTITY_REGEXP="^https://github.com/${REPO}/.github/workflows/release.yml@"

case "$NETWORK" in
  mainnet|testnet) ;;
  *) echo "Unknown NETWORK: $NETWORK (expected 'mainnet' or 'testnet')" >&2; exit 1 ;;
esac
TMP_DIR=""

cleanup() { [ -n "$TMP_DIR" ] && rm -rf "$TMP_DIR"; }
trap cleanup EXIT

detect_platform() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"

  case "$os" in
    Linux)  os="linux" ;;
    Darwin) os="macos" ;;
    *)      echo "Unsupported OS: $os" >&2; exit 1 ;;
  esac

  case "$arch" in
    x86_64|amd64)  arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *)             echo "Unsupported architecture: $arch" >&2; exit 1 ;;
  esac

  echo "${os}-${arch}"
}

get_latest_tag() {
  if [ "$NETWORK" = "testnet" ]; then
    curl -fsSL --connect-timeout 10 --max-time 30 \
      "https://api.github.com/repos/${REPO}/releases?per_page=30" 2>/dev/null |
      grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/' |
      grep '^testnet-' | head -1 || true
  else
    curl -fsSL --connect-timeout 10 --max-time 30 \
      "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null |
      grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/' || true
  fi
}

download_sums() {
  local tag="$1"
  local sums_url="https://github.com/${REPO}/releases/download/${tag}/SHA256SUMS"
  curl -fSL --connect-timeout 10 --max-time 30 -o "${TMP_DIR}/SHA256SUMS" "$sums_url"
  verify_sums_signature "$tag"
}

verify_sums_signature() {
  local tag="$1"
  local bundle_url="https://github.com/${REPO}/releases/download/${tag}/SHA256SUMS.sigstore.json"

  if ! command -v cosign &>/dev/null; then
    echo "cosign is required to verify the release signature but is not installed." >&2
    echo "Install it from https://docs.sigstore.dev/cosign/system_config/installation/ and re-run." >&2
    exit 1
  fi

  local http_code curl_status=0
  http_code="$(curl -fsSL --connect-timeout 10 --max-time 30 \
    -o "${TMP_DIR}/SHA256SUMS.sigstore.json" -w '%{http_code}' \
    "$bundle_url" 2>"${TMP_DIR}/curl_stderr")" || curl_status=$?
  if [ "$curl_status" -ne 0 ]; then
    case "$http_code" in
      4*)
        echo "Release ${tag} does not publish a SHA256SUMS.sigstore.json signature bundle (HTTP ${http_code})." >&2
        echo "Refusing to install an unsigned release." >&2
        ;;
      *)
        echo "Failed to fetch the signature bundle (curl exit ${curl_status}, HTTP ${http_code:-none}):" >&2
        cat "${TMP_DIR}/curl_stderr" >&2
        echo "Refusing to install without signature verification; re-run when the network is available." >&2
        ;;
    esac
    exit 1
  fi

  echo "Verifying SHA256SUMS signature against the ${REPO} release workflow identity..."
  if ! cosign verify-blob \
    --bundle "${TMP_DIR}/SHA256SUMS.sigstore.json" \
    --certificate-identity-regexp "$SIGNER_IDENTITY_REGEXP" \
    --certificate-oidc-issuer "$OIDC_ISSUER" \
    "${TMP_DIR}/SHA256SUMS"; then
    echo "Signature verification FAILED for SHA256SUMS (${tag})." >&2
    echo "Refusing to install. The release may have been tampered with." >&2
    exit 1
  fi
  echo "Signature verified."
}

download_and_verify() {
  local tag="$1" platform="$2" binary="$3"
  local asset="${binary}-${platform}"
  local url="https://github.com/${REPO}/releases/download/${tag}/${asset}"

  echo "Downloading ${asset} (${tag})..."
  curl -fSL --connect-timeout 10 --max-time 120 -o "${TMP_DIR}/${asset}" "$url"

  echo "Verifying checksum..."
  local expected actual
  expected="$(awk -v a="${asset}" '$2 == a {print $1}' "${TMP_DIR}/SHA256SUMS")"
  if [ -z "$expected" ]; then
    echo "Asset ${asset} not found in SHA256SUMS" >&2
    exit 1
  fi

  if command -v sha256sum &>/dev/null; then
    actual="$(sha256sum "${TMP_DIR}/${asset}" | awk '{print $1}')"
  else
    actual="$(shasum -a 256 "${TMP_DIR}/${asset}" | awk '{print $1}')"
  fi

  if [ "$expected" != "$actual" ]; then
    echo "Checksum mismatch for ${asset}" >&2
    echo "  expected: ${expected}" >&2
    echo "  actual:   ${actual}" >&2
    exit 1
  fi

  if [ -w "$INSTALL_DIR" ]; then
    install -m 755 "${TMP_DIR}/${asset}" "${INSTALL_DIR}/${binary}"
  else
    sudo install -m 755 "${TMP_DIR}/${asset}" "${INSTALL_DIR}/${binary}"
  fi

  echo "Installed ${binary} to ${INSTALL_DIR}/${binary}"
}

main() {
  local platform tag
  platform="$(detect_platform)"
  echo "Detected platform: ${platform}"

  tag="$(get_latest_tag)"
  if [ -z "$tag" ]; then
    echo "Could not determine latest release" >&2
    exit 1
  fi

  if ! [ -w "$INSTALL_DIR" ]; then
    echo "Requesting sudo for install to ${INSTALL_DIR}..."
    sudo -v
  fi

  TMP_DIR="$(mktemp -d)"
  download_sums "$tag"

  case "$BINARY" in
    all)
      download_and_verify "$tag" "$platform" "sn2-miner"
      download_and_verify "$tag" "$platform" "sn2-validator"
      ;;
    sn2-miner|sn2-validator)
      download_and_verify "$tag" "$platform" "$BINARY"
      ;;
    *)
      echo "Usage: install.sh [sn2-miner|sn2-validator|all]" >&2
      exit 1
      ;;
  esac
}

main
