#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TOOL_FILE="$ROOT/.github/tool-versions.json"
INSTALL_ROOT="${OTTO_TOOL_INSTALL_DIR:-$ROOT/.phase80/tools}"
BIN_DIR="$INSTALL_ROOT/bin"
VERIFY_ONLY=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --verify-only)
      VERIFY_ONLY=1
      shift
      ;;
    --install-root)
      [[ $# -ge 2 ]] || { echo "--install-root requires a path" >&2; exit 2; }
      INSTALL_ROOT="$2"
      BIN_DIR="$INSTALL_ROOT/bin"
      shift 2
      ;;
    -h|--help)
      echo "usage: install-pinned-tools.sh [--verify-only] [--install-root DIR]"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

platform_key() {
  case "$(uname -s):$(uname -m)" in
    Darwin:arm64) echo "darwin_arm64" ;;
    *) echo "unsupported platform: $(uname -s) $(uname -m)" >&2; exit 2 ;;
  esac
}

json_field() {
  local tool="$1" field="$2"
  python3 - "$TOOL_FILE" "$tool" "$field" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    document = json.load(stream)
value = document["tools"][sys.argv[2]]
for part in sys.argv[3].split("."):
    value = value[part]
print(value)
PY
}

json_asset_field() {
  local tool="$1" field="$2" platform
  platform="$(platform_key)"
  python3 - "$TOOL_FILE" "$tool" "$platform" "$field" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    document = json.load(stream)
print(document["tools"][sys.argv[2]]["assets"][sys.argv[3]][sys.argv[4]])
PY
}

download_verified() {
  local url="$1" sha="$2" output="$3"
  curl --fail --location --silent --show-error "$url" --output "$output"
  printf '%s  %s\n' "$sha" "$output" | shasum -a 256 -c -
}

install_cargo_crate() {
  local tool="$1" version url sha tmp crate_dir
  version="$(json_field "$tool" version)"
  url="$(json_field "$tool" source.url)"
  sha="$(json_field "$tool" source.sha256)"
  tmp="$(mktemp -d)"
  download_verified "$url" "$sha" "$tmp/$tool.crate"
  mkdir -p "$tmp/src"
  tar -xzf "$tmp/$tool.crate" -C "$tmp/src"
  crate_dir="$tmp/src/$tool-$version"
  cargo install --locked --path "$crate_dir" --root "$INSTALL_ROOT"
}

install_direct_binary() {
  local tool="$1" url sha binary tmp
  url="$(json_asset_field "$tool" url)"
  sha="$(json_asset_field "$tool" sha256)"
  binary="$(json_asset_field "$tool" binary)"
  tmp="$(mktemp -d)"
  download_verified "$url" "$sha" "$tmp/$binary"
  install -m 0755 "$tmp/$binary" "$BIN_DIR/$binary"
}

install_tar_binary() {
  local tool="$1" url sha binary tmp found
  url="$(json_asset_field "$tool" url)"
  sha="$(json_asset_field "$tool" sha256)"
  binary="$(json_asset_field "$tool" binary)"
  tmp="$(mktemp -d)"
  download_verified "$url" "$sha" "$tmp/$tool.tar.gz"
  mkdir -p "$tmp/extract"
  tar -xzf "$tmp/$tool.tar.gz" -C "$tmp/extract"
  found="$(find "$tmp/extract" -type f -name "$binary" -perm -u+x -print -quit)"
  if [[ -z "$found" ]]; then
    found="$(find "$tmp/extract" -type f -name "$binary" -print -quit)"
  fi
  [[ -n "$found" ]] || { echo "could not find $binary in $tool archive" >&2; exit 1; }
  install -m 0755 "$found" "$BIN_DIR/$binary"
}

tool_version_output() {
  local command="$1"
  case "$command" in
    gitleaks) "$command" version 2>&1 | head -n 1 ;;
    *) "$command" --version 2>&1 | head -n 1 ;;
  esac
}

verify_tool() {
  local tool="$1" command version output
  command="$(json_field "$tool" command)"
  version="$(json_field "$tool" version)"
  if ! command -v "$command" >/dev/null 2>&1; then
    echo "$command is missing from PATH" >&2
    return 1
  fi
  output="$(tool_version_output "$command")"
  if [[ "$output" != *"$version"* ]]; then
    echo "$command version mismatch: expected $version, got $output" >&2
    return 1
  fi
  echo "$command: $output"
}

TOOLS=(cargo-deny cargo-public-api osv-scanner gitleaks actionlint zizmor)

if [[ "$VERIFY_ONLY" -eq 0 ]]; then
  mkdir -p "$BIN_DIR"
  for tool in "${TOOLS[@]}"; do
    kind="$(json_field "$tool" kind)"
    case "$kind" in
      cargo_crate) install_cargo_crate "$tool" ;;
      direct_binary) install_direct_binary "$tool" ;;
      tar_binary) install_tar_binary "$tool" ;;
      *) echo "unknown tool kind for $tool: $kind" >&2; exit 2 ;;
    esac
  done
fi

export PATH="$BIN_DIR:$PATH"
for tool in "${TOOLS[@]}"; do
  verify_tool "$tool"
done
