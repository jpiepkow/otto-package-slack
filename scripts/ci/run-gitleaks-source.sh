#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
REPORT=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --report)
      [[ $# -ge 2 ]] || { echo "--report requires a path" >&2; exit 2; }
      REPORT="$2"
      shift 2
      ;;
    -h|--help)
      echo "usage: run-gitleaks-source.sh [--report PATH]"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

TMP="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP"
}
trap cleanup EXIT

python3 - "$ROOT" "$TMP/source" <<'PY'
from __future__ import annotations

from pathlib import Path
import shutil
import subprocess
import sys

root = Path(sys.argv[1])
dest = Path(sys.argv[2])
dest.mkdir(parents=True, exist_ok=True)

result = subprocess.run(
    ["git", "-C", str(root), "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
    check=True,
    stdout=subprocess.PIPE,
)

for rel in [item for item in result.stdout.decode("utf-8").split("\0") if item]:
    source = root / rel
    if not source.is_file() or source.is_symlink():
        continue
    target = dest / rel
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, target)
PY

if [[ -n "$REPORT" ]]; then
  mkdir -p "$(dirname "$REPORT")"
  gitleaks dir --config "$ROOT/.gitleaks.toml" --no-banner --redact \
    --report-format json --report-path "$REPORT" "$TMP/source"
  python3 - "$REPORT" <<'PY'
from __future__ import annotations

import json
from pathlib import Path
import sys

report = Path(sys.argv[1])
if not report.exists():
    raise SystemExit(f"gitleaks report was not created: {report}")
findings = json.loads(report.read_text(encoding="utf-8") or "[]")
if findings:
    raise SystemExit(f"gitleaks source scan found {len(findings)} finding(s)")
PY
else
  gitleaks dir --config "$ROOT/.gitleaks.toml" --no-banner --redact "$TMP/source"
fi
