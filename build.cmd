cd /d "%~dp0"

python tools\generate_licenses.py cdfuse\Cargo.toml  cdfuse\THIRD_PARTY_LICENSES.md
python tools\generate_licenses.py cdwinfs\Cargo.toml cdwinfs\THIRD_PARTY_LICENSES.md
python tools\generate_licenses.py cdcore\Cargo.toml  cdcore\THIRD_PARTY_LICENSES.md

cargo build --release --manifest-path cdcore\Cargo.toml
cargo build --release --manifest-path cdwinfs\Cargo.toml
