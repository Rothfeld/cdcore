#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"

cargo run --bin stub_gen --features python
cp python/crimsonforge_core.pyi python/crimsonforge_core/__init__.pyi

maturin build --release --features python
rm -f target/wheels/*linux_x86_64.whl
pip install --force-reinstall --quiet target/wheels/*.whl
python3 -c "import crimsonforge_core as cf; print(f'installed {cf.__version__}')"
