#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/../.."

expected_rust="1.97.0"
expected_msrv="1.88.0"
actual_rust="$(awk -F'"' '/channel/ { print $2; exit }' rust-toolchain.toml)"
actual_msrv="$(awk -F'"' '/rust-version/ { print $2; exit }' Cargo.toml)"

if [[ "${actual_rust}" != "${expected_rust}" ]]; then
  echo "unsupported rust toolchain: expected ${expected_rust}, actual ${actual_rust}" >&2
  exit 1
fi

if [[ "${actual_msrv}" != "${expected_msrv}" ]]; then
  echo "unsupported MSRV: expected ${expected_msrv}, actual ${actual_msrv}" >&2
  exit 1
fi

python3 - <<'INNER_PY'
from pathlib import Path
import tomllib

for filename in ("Cargo.toml", "otto.toml"):
    path = Path(filename)
    if not path.is_file():
        raise SystemExit(f"missing required manifest: {filename}")
    tomllib.loads(path.read_text())
INNER_PY

cargo "+${expected_rust}" fmt --all -- --check
cargo "+${expected_msrv}" check --all-targets --all-features
cargo "+${expected_rust}" clippy --all-targets --all-features
cargo "+${expected_rust}" build --bins --all-features
cargo "+${expected_rust}" test --all-targets --all-features
