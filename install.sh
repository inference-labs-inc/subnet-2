#!/usr/bin/env bash
set -euo pipefail

REPO="inference-labs-inc/subnet-2"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
BINARY="${1:-all}"
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
  curl -fsSL --connect-timeout 10 --max-time 30 "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null | grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/' || true
}

download_sums() {
  local tag="$1"
  local sums_url="https://github.com/${REPO}/releases/download/${tag}/SHA256SUMS"
  curl -fSL --connect-timeout 10 --max-time 30 -o "${TMP_DIR}/SHA256SUMS" "$sums_url"
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
