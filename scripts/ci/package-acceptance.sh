#!/usr/bin/env bash
set -euo pipefail

readonly CONTRACT_VERSION="otto.package.acceptance.v1"
readonly DESCRIPTOR_PATH="tests/package-acceptance.json"
readonly EXPECTED_RUST="1.97.0"
readonly EXPECTED_MSRV="1.88.0"
readonly -a CONTRACT_STEPS=(
  "manifest_schema"
  "exact_toolchain_and_msrv"
  "fmt_clippy_build_tests"
  "runtime_negotiation"
  "fake_capability_cases"
  "negative_protocol_cases"
  "timeout_cancel_or_duplicate_cases"
  "redaction_or_secret_safety"
  "offline_boundary"
  "no_skips_or_unavailable_fixtures"
)

usage() {
  cat <<'USAGE'
usage: scripts/ci/package-acceptance.sh [--self-test|--print-contract|--contract-hash]

Run Otto's deterministic package acceptance contract from a package repository
root. Package-specific behavior is declared in tests/package-acceptance.json;
this runner owns pass/fail semantics and does not allow package wrappers to
downgrade missing fixtures into skips.
USAGE
}

contract_hash() {
  python3 - "$CONTRACT_VERSION" "${CONTRACT_STEPS[@]}" <<'PY'
import hashlib
import json
import sys

payload = {
    "contract_version": sys.argv[1],
    "steps": sys.argv[2:],
}
encoded = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()
print(hashlib.sha256(encoded).hexdigest())
PY
}

print_contract() {
  local hash
  hash="$(contract_hash)"
  printf 'contract_version: %s\n' "$CONTRACT_VERSION"
  printf 'contract_hash: %s\n' "$hash"
  printf 'descriptor: %s\n' "$DESCRIPTOR_PATH"
  printf 'required_steps:\n'
  printf '  - %s\n' "${CONTRACT_STEPS[@]}"
}

validate_descriptor() {
  local hash
  hash="$(contract_hash)"
  OTTO_PACKAGE_ACCEPTANCE_CONTRACT_VERSION="$CONTRACT_VERSION" \
  OTTO_PACKAGE_ACCEPTANCE_CONTRACT_HASH="$hash" \
  OTTO_PACKAGE_ACCEPTANCE_DESCRIPTOR="$DESCRIPTOR_PATH" \
  python3 - <<'PY'
from __future__ import annotations

import json
import os
from pathlib import Path, PurePosixPath
import sys
import tomllib
from typing import Any

REQUIRED_CATEGORIES = [
    "manifest_schema",
    "runtime_negotiation",
    "fake_capability_cases",
    "negative_protocol_cases",
    "timeout_cancel_or_duplicate_cases",
    "redaction_or_secret_safety",
    "offline_boundary",
]
FORBIDDEN_STATUS = {"skip", "skipped", "unavailable", "todo", "pending"}


def fail(message: str) -> None:
    print(f"error: {message}", file=sys.stderr)
    raise SystemExit(1)


def load_toml(path: Path) -> dict[str, Any]:
    if not path.is_file():
        fail(f"missing required file: {path}")
    try:
        return tomllib.loads(path.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as error:
        fail(f"{path} did not parse as TOML: {error}")


def load_json(path: Path) -> dict[str, Any]:
    if not path.is_file():
        fail(f"missing required acceptance descriptor: {path}")
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        fail(f"{path} did not parse as JSON: {error}")
    if not isinstance(value, dict):
        fail(f"{path} must contain a JSON object")
    return value


def normalized_relative(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value:
        fail(f"{field} must be a nonempty relative path")
    if value.startswith("/") or "\\" in value:
        fail(f"{field} must be a relative POSIX path")
    parts = value.split("/")
    if any(part in {"", ".", ".."} for part in parts):
        fail(f"{field} contains an empty, dot, or dot-dot component")
    if PurePosixPath(value).as_posix() != value:
        fail(f"{field} is not normalized")
    return value


def string_list(value: Any, field: str) -> list[str]:
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        fail(f"{field} must be a list of strings")
    if len(value) != len(set(value)):
        fail(f"{field} contains duplicates")
    return value


def assert_exact(actual: list[str], expected: list[str], field: str) -> None:
    if actual != expected:
        fail(f"{field} mismatch: expected {expected}, actual {actual}")


def reject_skips(value: Any, path: str = "descriptor") -> None:
    if isinstance(value, dict):
        for key, child in value.items():
            reject_skips(child, f"{path}.{key}")
    elif isinstance(value, list):
        for index, child in enumerate(value):
            reject_skips(child, f"{path}[{index}]")
    elif isinstance(value, str) and value.strip().lower() in FORBIDDEN_STATUS:
        fail(f"{path} uses forbidden non-green status {value!r}")


def manifest_ids(items: Any, field: str) -> list[str]:
    if items is None:
        return []
    if not isinstance(items, list):
        fail(f"otto.toml {field} must be a list")
    ids: list[str] = []
    for index, item in enumerate(items):
        if not isinstance(item, dict) or not isinstance(item.get("id"), str):
            fail(f"otto.toml {field}[{index}] is missing string id")
        ids.append(item["id"])
    return ids


def validate_evidence_paths(fixtures: Any) -> None:
    if not isinstance(fixtures, dict):
        fail("fixture_categories must be an object")
    actual_categories = sorted(fixtures)
    assert_exact(actual_categories, sorted(REQUIRED_CATEGORIES), "fixture_categories")
    for category in REQUIRED_CATEGORIES:
        paths = string_list(fixtures.get(category), f"fixture_categories.{category}")
        if not paths:
            fail(f"fixture_categories.{category} must name at least one evidence path")
        for raw_path in paths:
            path = Path(normalized_relative(raw_path, f"fixture_categories.{category}[]"))
            if not path.exists():
                fail(f"fixture evidence path does not exist: {raw_path}")


cargo = load_toml(Path("Cargo.toml"))
manifest = load_toml(Path("otto.toml"))
descriptor_path = Path(os.environ["OTTO_PACKAGE_ACCEPTANCE_DESCRIPTOR"])
descriptor = load_json(descriptor_path)
reject_skips(descriptor)

if descriptor.get("schema_version") != 1:
    fail("descriptor schema_version must equal 1")
if descriptor.get("contract_version") != os.environ["OTTO_PACKAGE_ACCEPTANCE_CONTRACT_VERSION"]:
    fail("descriptor contract_version does not match runner")
if descriptor.get("contract_hash") != os.environ["OTTO_PACKAGE_ACCEPTANCE_CONTRACT_HASH"]:
    fail("descriptor contract_hash does not match runner")
if descriptor.get("package_id") != manifest.get("package_id"):
    fail("descriptor package_id does not match otto.toml")
if manifest.get("schema_version") != 1:
    fail("otto.toml schema_version must equal 1")
if manifest.get("protocol_version") != "otto.extension.rpc.v1":
    fail("otto.toml protocol_version must equal otto.extension.rpc.v1")

package = cargo.get("package")
if not isinstance(package, dict):
    fail("Cargo.toml missing [package]")
if package.get("rust-version") != "1.88.0":
    fail("Cargo.toml rust-version must equal 1.88.0")

provides = manifest.get("provides", {}) or {}
if not isinstance(provides, dict):
    fail("otto.toml provides must be a table when present")
for section, declaration in provides.items():
    if not isinstance(declaration, dict) or declaration.get("version") != 1:
        fail(f"provides.{section}.version must equal 1")
assert_exact(
    string_list(descriptor.get("provides"), "provides"),
    sorted(provides),
    "provides",
)
assert_exact(
    string_list(descriptor.get("capabilities"), "capabilities"),
    sorted(manifest_ids(manifest.get("capabilities"), "capabilities")),
    "capabilities",
)
assert_exact(
    string_list(descriptor.get("tool_cases"), "tool_cases"),
    sorted(manifest_ids(manifest.get("tools"), "tools")),
    "tool_cases",
)
assert_exact(
    string_list(descriptor.get("trigger_cases"), "trigger_cases"),
    sorted(manifest_ids(manifest.get("triggers"), "triggers")),
    "trigger_cases",
)
assert_exact(
    string_list(descriptor.get("setup_check_cases"), "setup_check_cases"),
    sorted(manifest_ids(manifest.get("setup_checks"), "setup_checks")),
    "setup_check_cases",
)

runtime = manifest.get("runtime")
if not isinstance(runtime, dict) or not isinstance(runtime.get("command"), str) or not runtime["command"]:
    fail("otto.toml runtime.command must be nonempty")
if descriptor.get("runtime_command") != runtime["command"]:
    fail("descriptor runtime_command does not match otto.toml")

offline = descriptor.get("offline")
if not isinstance(offline, dict):
    fail("offline must be an object")
if offline.get("requires_live_network") is not False:
    fail("offline.requires_live_network must be false")
if offline.get("requires_live_credentials") is not False:
    fail("offline.requires_live_credentials must be false")
if offline.get("requires_host_mutation") is not False:
    fail("offline.requires_host_mutation must be false")

validate_evidence_paths(descriptor.get("fixture_categories"))
print(
    json.dumps(
        {
            "status": "valid",
            "contract_version": descriptor["contract_version"],
            "contract_hash": descriptor["contract_hash"],
            "package_id": descriptor["package_id"],
        },
        sort_keys=True,
    )
)
PY
}

check_versions() {
  local actual_rust actual_msrv
  actual_rust="$(awk -F'"' '/channel/ { print $2; exit }' rust-toolchain.toml)"
  actual_msrv="$(awk -F'"' '/rust-version/ { print $2; exit }' Cargo.toml)"

  if [[ "$actual_rust" != "$EXPECTED_RUST" ]]; then
    echo "unsupported rust toolchain: expected $EXPECTED_RUST, actual $actual_rust" >&2
    exit 1
  fi
  if [[ "$actual_msrv" != "$EXPECTED_MSRV" ]]; then
    echo "unsupported MSRV: expected $EXPECTED_MSRV, actual $actual_msrv" >&2
    exit 1
  fi
}

check_dependency_sources() {
  if command -v rg >/dev/null 2>&1; then
    if rg -n 'ssh://|git@github|path *=|branch *=|tag *=' Cargo.toml Cargo.lock 2>/dev/null; then
      echo "disallowed private, local, or floating dependency source" >&2
      exit 1
    fi
  else
    if grep -En 'ssh://|git@github|path *=|branch *=|tag *=' Cargo.toml Cargo.lock 2>/dev/null; then
      echo "disallowed private, local, or floating dependency source" >&2
      exit 1
    fi
  fi
}

run_acceptance() {
  cd "$(dirname "$0")/../.."
  export OTTO_FLEET_OFFLINE="${OTTO_FLEET_OFFLINE:-1}"
  export OTTO_ACCEPTANCE_FAKE_MODE="${OTTO_ACCEPTANCE_FAKE_MODE:-1}"

  check_versions
  check_dependency_sources
  validate_descriptor

  cargo "+${EXPECTED_RUST}" fmt --all -- --check
  cargo "+${EXPECTED_MSRV}" check --all-targets --all-features
  cargo "+${EXPECTED_RUST}" clippy --all-targets --all-features
  cargo "+${EXPECTED_RUST}" build --bins --all-features
  cargo "+${EXPECTED_RUST}" test --all-targets --all-features
}

expect_failure() {
  if "$@" >/dev/null 2>&1; then
    echo "error: self-test expected failure: $*" >&2
    exit 1
  fi
}

self_test() {
  local temp hash self_path
  temp="$(mktemp -d "${TMPDIR:-/tmp}/otto-package-acceptance.XXXXXX")"
  trap 'rm -rf "${temp:-}"' EXIT
  hash="$(contract_hash)"
  self_path="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

  mkdir -p "$temp/tests" "$temp/bin"
  cat >"$temp/Cargo.toml" <<'TOML'
[package]
name = "acceptance-fixture"
version = "0.1.0"
edition = "2024"
rust-version = "1.88.0"
TOML
  cat >"$temp/otto.toml" <<'TOML'
schema_version = 1
package_id = "com.otto.acceptance-fixture"
display_name = "Acceptance Fixture"
protocol_version = "otto.extension.rpc.v1"
roles = []
schemas = []
tools = []
triggers = []
setup_checks = []
ui_forms = []
migrations = []
redaction = []
capabilities = []

[runtime]
command = "bin/fixture"
args = []
idle_timeout_ms = 30000
health_timeout_ms = 2000

[provides.tools]
version = 1
TOML
  for category in "${CONTRACT_STEPS[@]}"; do
    touch "$temp/tests/$category.evidence"
  done
  python3 - "$temp/$DESCRIPTOR_PATH" "$hash" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
path.parent.mkdir(parents=True, exist_ok=True)
required = [
    "manifest_schema",
    "runtime_negotiation",
    "fake_capability_cases",
    "negative_protocol_cases",
    "timeout_cancel_or_duplicate_cases",
    "redaction_or_secret_safety",
    "offline_boundary",
]
descriptor = {
    "schema_version": 1,
    "contract_version": "otto.package.acceptance.v1",
    "contract_hash": sys.argv[2],
    "package_id": "com.otto.acceptance-fixture",
    "runtime_command": "bin/fixture",
    "provides": ["tools"],
    "capabilities": [],
    "tool_cases": [],
    "trigger_cases": [],
    "setup_check_cases": [],
    "offline": {
        "requires_live_network": False,
        "requires_live_credentials": False,
        "requires_host_mutation": False,
    },
    "fixture_categories": {
        category: [f"tests/{category}.evidence"] for category in required
    },
}
path.write_text(json.dumps(descriptor, indent=2, sort_keys=True) + "\n")
PY

  (cd "$temp" && validate_descriptor >/dev/null)
  rm "$temp/tests/offline_boundary.evidence"
  expect_failure bash -c "cd '$temp' && '$self_path' --validate-only"
  # Recreate and validate the normal missing-evidence path through this process.
  touch "$temp/tests/offline_boundary.evidence"
  (cd "$temp" && validate_descriptor >/dev/null)
  python3 - "$temp/$DESCRIPTOR_PATH" <<'PY'
import json
import pathlib
import sys
path = pathlib.Path(sys.argv[1])
descriptor = json.loads(path.read_text())
descriptor["fixture_categories"]["offline_boundary"] = []
path.write_text(json.dumps(descriptor, indent=2, sort_keys=True) + "\n")
PY
  expect_failure bash -c "cd '$temp' && '$self_path' --validate-only"
  echo "package-acceptance self-test: PASS"
}

case "${1:-}" in
  "")
    run_acceptance
    ;;
  --self-test)
    [[ $# -eq 1 ]] || { usage >&2; exit 2; }
    self_test
    ;;
  --print-contract)
    [[ $# -eq 1 ]] || { usage >&2; exit 2; }
    print_contract
    ;;
  --contract-hash)
    [[ $# -eq 1 ]] || { usage >&2; exit 2; }
    contract_hash
    ;;
  --validate-only)
    [[ $# -eq 1 ]] || { usage >&2; exit 2; }
    validate_descriptor
    ;;
  -h|--help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
