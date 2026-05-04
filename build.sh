#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

cargo fetch --manifest-path cdfuse/Cargo.toml
cargo fetch --manifest-path cdcore/Cargo.toml

python tools/generate_licenses.py cdfuse/Cargo.toml cdfuse/THIRD_PARTY_LICENSES.md
python tools/generate_licenses.py cdcore/Cargo.toml cdcore/THIRD_PARTY_LICENSES.md

cargo run --release --manifest-path cdcore/Cargo.toml --bin stub_gen --features python
cp cdcore/python/cdcore.pyi cdcore/python/cdcore/__init__.pyi
maturin build --release --manifest-path cdcore/Cargo.toml --features python
cargo build --release --manifest-path cdfuse/Cargo.toml
