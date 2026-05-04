#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

cargo fetch --manifest-path cdfuse/Cargo.toml
cargo fetch --manifest-path cdcore/Cargo.toml

python tools/generate_licenses.py cdfuse/Cargo.toml cdfuse/THIRD_PARTY_LICENSES.md
python tools/generate_licenses.py cdcore/Cargo.toml cdcore/THIRD_PARTY_LICENSES.md

cargo build --release --manifest-path cdcore/Cargo.toml
cargo build --release --manifest-path cdfuse/Cargo.toml
