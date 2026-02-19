#!/usr/bin/env bash
set -euo pipefail

REPO="inference-labs-inc/subnet-2"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"
BINARY="${1:-all}"

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
  curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | sed -E 's/.*"([^"]+)".*/\1/'
}

download_and_verify() {
  local tag="$1" platform="$2" binary="$3"
  local asset="${binary}-${platform}"
  local url="https://github.com/${REPO}/releases/download/${tag}/${asset}"
  local sums_url="https://github.com/${REPO}/releases/download/${tag}/SHA256SUMS"
  local tmp
  tmp="$(mktemp -d)"

  echo "Downloading ${asset} (${tag})..."
  curl -fSL -o "${tmp}/${asset}" "$url"
  curl -fSL -o "${tmp}/SHA256SUMS" "$sums_url"

  echo "Verifying checksum..."
  local expected actual
  expected="$(grep "${asset}" "${tmp}/SHA256SUMS" | awk '{print $1}')"
  if [ -z "$expected" ]; then
    echo "Asset ${asset} not found in SHA256SUMS" >&2
    rm -rf "$tmp"
    exit 1
  fi

  if command -v sha256sum &>/dev/null; then
    actual="$(sha256sum "${tmp}/${asset}" | awk '{print $1}')"
  else
    actual="$(shasum -a 256 "${tmp}/${asset}" | awk '{print $1}')"
  fi

  if [ "$expected" != "$actual" ]; then
    echo "Checksum mismatch for ${asset}" >&2
    echo "  expected: ${expected}" >&2
    echo "  actual:   ${actual}" >&2
    rm -rf "$tmp"
    exit 1
  fi

  chmod +x "${tmp}/${asset}"

  if [ -w "$INSTALL_DIR" ]; then
    mv "${tmp}/${asset}" "${INSTALL_DIR}/${binary}"
  else
    sudo mv "${tmp}/${asset}" "${INSTALL_DIR}/${binary}"
  fi

  rm -rf "$tmp"
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
