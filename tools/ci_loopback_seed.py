#!/usr/bin/env python3
"""Download a single PROOF_OF_COMPUTATION JSTPROVE circuit for loopback CI testing.

Fetches the first eligible circuit from the circuit repository API, downloads
model.compiled and settings.json to the standard circuit cache directory, and
prints the circuit ID to stdout.

Usage:
    python tools/ci_loopback_seed.py [--force]
"""

import argparse
import json
import os
import sys
import urllib.parse
import urllib.request
from pathlib import Path

CIRCUIT_API_URL = os.environ.get("CIRCUIT_API_URL", "https://repository.inferencelabs.com")
CIRCUIT_CACHE_DIR = Path.home() / ".bittensor" / "subnet-2" / "circuit_cache"
CIRCUIT_METADATA_FILENAME = "circuit_metadata.json"

IGNORED = {
    "0",
    "0a92bc32ea02abe54159da70aeb541d52c3cba27c8708669eda634e096a86f8b",
    "b7d33e7c19360c042d94c5a7360d7dc68c36dd56c449f7c49164a0098769c01f",
    "55de10a6bcf638af4bc79901d63204a9e5b1c6534670aa03010bae6045e3d0e8",
    "9998a12b8194d3e57d332b484ede57c3d871d42a176456c4e10da2995791d181",
    "ed8ba401d709ee31f6b9272163c71451da171c7d71800313fe5db58d0f6c483a",
    "37320fc74fec80805eedc8e92baf3c58842a2cb2a4ae127ad6e930f0c8441c7a",
    "1d60d545b7c5123fd60524dcbaf57081ca7dc4a9ec36c892927a3153328d17c0",
    "33b92394b18412622adad75733a6fc659b4e202b01ee8a5465958a6bad8ded62",
    "8dcff627a782525ea86196941a694ffbead179905f0cd4550ddc3df9e2b90924",
    "a4bcecaf699fd9212600a1f2fcaa40c444e1aeaab409ea240a38c33ed356f4e2",
    "e84b2e5f223621fa20078eb9f920d8d4d3a4ff95fa6e2357646fdbb43a2557c9",
    "a849500803abdbb86a9460e18684a6411dc7ae0b75f1f6330e3028081a497dea",
    "f5b6043594f46ae6bd176ce60c7a099291cc6a3f6436fecd46142b1b1ecca5fb",
    "1e6fcdaea58741e7248b631718dda90398a17b294480beb12ce8232e27ca3bff",
    "fa0d509d52abe2d1e809124f8aba46258a02f7253582f7b7f5a22e1e0bca0dfb",
}

SKIP_DOWNLOAD = {"metadata.json", "full_model.onnx"}
REQUIRED_FILES = {"model.compiled", "settings.json"}


def _require_https(url: str) -> None:
    if urllib.parse.urlparse(url).scheme != "https":
        raise ValueError(f"refusing non-HTTPS URL: {url}")


def fetch_json(url: str) -> dict:
    _require_https(url)
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.loads(resp.read())


def download_file(url: str, dest: Path) -> None:
    _require_https(url)
    dest.parent.mkdir(parents=True, exist_ok=True)
    req = urllib.request.Request(url)
    total = 0
    with urllib.request.urlopen(req, timeout=300) as resp, dest.open("wb") as fh:
        while chunk := resp.read(65536):
            fh.write(chunk)
            total += len(chunk)
    print(f"  {dest.name}: {total:,} bytes", file=sys.stderr)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--force", action="store_true", help="Re-download even if cached")
    args = parser.parse_args()

    print(f"Fetching circuit list from {CIRCUIT_API_URL} ...", file=sys.stderr)
    data = fetch_json(f"{CIRCUIT_API_URL}/circuits")
    circuits = data.get("circuits", [])

    chosen = None
    for c in circuits:
        cid = c.get("id", "")
        if not cid or cid in IGNORED:
            continue
        meta = c.get("metadata", {})
        if meta.get("type") != "PROOF_OF_COMPUTATION":
            continue
        if meta.get("proof_system") != "JSTPROVE":
            continue
        files = c.get("files", {})
        if "model.compiled" not in files or "settings.json" not in files:
            continue
        chosen = c
        break

    if chosen is None:
        print("No eligible PROOF_OF_COMPUTATION JSTPROVE circuit found", file=sys.stderr)
        sys.exit(1)

    circuit_id = chosen["id"]
    metadata = chosen["metadata"]
    files = chosen.get("files", {})

    print(
        f"Selected: {circuit_id[:16]}... ({metadata.get('name', 'unnamed')})",
        file=sys.stderr,
    )

    model_dir = CIRCUIT_CACHE_DIR / f"model_{circuit_id}"
    model_dir.mkdir(parents=True, exist_ok=True)

    metadata_path = model_dir / CIRCUIT_METADATA_FILENAME
    metadata_path.write_text(json.dumps(metadata, indent=2))
    print(f"  {CIRCUIT_METADATA_FILENAME}: written", file=sys.stderr)

    for filename in REQUIRED_FILES:
        url = files.get(filename)
        if not url:
            print(f"  {filename}: not in API response, skipping", file=sys.stderr)
            continue
        dest = model_dir / filename
        if dest.exists() and not args.force:
            print(f"  {filename}: cached ({dest.stat().st_size:,} bytes)", file=sys.stderr)
            continue
        print(f"  {filename}: downloading ...", file=sys.stderr)
        download_file(url, dest)

    missing = [f for f in REQUIRED_FILES if not (model_dir / f).exists()]
    if missing:
        print(f"Missing required files after download: {missing}", file=sys.stderr)
        sys.exit(1)

    print(f"Circuit seeded to {model_dir}", file=sys.stderr)
    print(circuit_id)


if __name__ == "__main__":
    main()
