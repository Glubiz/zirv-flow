#!/usr/bin/env bash
# Checks Dockerfile image digests and the SECURITY.md review for each cargo-deny ignore.
# Usage: check-dependency-policy.sh [--self-test]
# Requires Python 3.11+ (stdlib TOML parser); performs no builds or network requests.

set -euo pipefail

if [ "$#" -gt 1 ] || { [ "$#" -eq 1 ] && [ "$1" != "--self-test" ]; }; then
  echo "usage: $0 [--self-test]" >&2
  exit 2
fi

run_gate() {
  python3 - "$1" <<'PY'
from pathlib import Path
import re
import sys
import tomllib

root = Path(sys.argv[1])
try:
    dockerfile = (root / "Dockerfile").read_text()
    config = tomllib.loads((root / "deny.toml").read_text())
    security = (root / "SECURITY.md").read_text()
except (OSError, tomllib.TOMLDecodeError) as error:
    print(f"dependency-policy gate: {error}", file=sys.stderr)
    sys.exit(1)

errors = []
stages = 0
# Docker ignores comment lines, including a backslash at the end of a comment.
instructions = []
pending = ""
for line in dockerfile.splitlines():
    if line.lstrip().startswith("#"):
        continue
    line = pending + line.rstrip()
    if line.endswith("\\"):
        pending = line[:-1] + " "
        continue
    instructions.append(line)
    pending = ""
if pending:
    instructions.append(pending)

for line in instructions:
    fields = line.split()
    if not fields or fields[0].upper() != "FROM":
        continue
    stages += 1
    fields = fields[1:]
    while fields and fields[0].startswith("--"):
        fields = fields[1:]
    image = fields[0] if fields else ""
    if not re.fullmatch(r"[^\s@]+@sha256:[0-9a-f]{64}", image):
        errors.append(f"Dockerfile: FROM image must carry an @sha256: digest: {line.strip()}")
if not stages:
    errors.append("Dockerfile: no FROM instructions found")

documented = set(re.findall(r"\bRUSTSEC-\d{4}-\d{4}\b", security))
for entry in config.get("advisories", {}).get("ignore", []):
    if not isinstance(entry, dict) or not isinstance(entry.get("id"), str):
        errors.append("deny.toml: each advisory ignore needs an id and reason")
        continue
    advisory = entry["id"]
    if not isinstance(entry.get("reason"), str) or not entry["reason"].strip():
        errors.append(f"deny.toml: {advisory} needs a reason")
    if advisory not in documented:
        errors.append(f"SECURITY.md: missing review for {advisory}")

if errors:
    for error in errors:
        print(f"dependency-policy gate: {error}", file=sys.stderr)
    sys.exit(1)
print(f"dependency-policy gate: {stages} pinned FROM instructions; all advisory ignores documented")
PY
}

run_self_test() {
  scratch="$(mktemp -d)"
  trap 'rm -rf "$scratch"' EXIT
  failures=0
  digest="$(printf '%064d' 0)"

  make_fixture() {
    cat >"$scratch/Dockerfile" <<EOF
# FROM ignored-comment
FROM --platform=\$BUILDPLATFORM node:22@sha256:$digest AS web
from rust:1.98@sha256:$digest AS builder
FROM \\
  runtime:latest@sha256:$digest
EOF
    cat >"$scratch/deny.toml" <<'EOF'
[advisories]
# ignore = ["RUSTSEC-2000-9999"]
ignore = [
    { id = "RUSTSEC-2000-0001", reason = "public keys only" },
    { id = 'RUSTSEC-2000-0002', reason = "no affected operation" },
]
EOF
    printf '%s\n' '# Known advisories' 'RUSTSEC-2000-0001' 'RUSTSEC-2000-0002' >"$scratch/SECURITY.md"
  }

  check_fixture() {
    name="$1"
    expected_exit="$2"
    expected_text="$3"
    actual_exit=0
    output="$(run_gate "$scratch" 2>&1)" || actual_exit=$?
    if [ "$actual_exit" -eq "$expected_exit" ] && [[ "$output" == *"$expected_text"* ]]; then
      echo "self-test ok: $name (exit $actual_exit)"
    else
      echo "self-test FAILED: $name (expected exit $expected_exit, got $actual_exit)" >&2
      printf '%s\n' "$output" | sed 's/^/    /' >&2
      failures=$((failures + 1))
    fi
  }

  make_fixture
  check_fixture "pinned-stages-and-reviewed-ignores" 0 "3 pinned FROM instructions"
  printf '%s\n' 'FROM runtime:latest' >>"$scratch/Dockerfile"
  check_fixture "unpinned-final-stage" 1 "FROM image must carry an @sha256: digest"
  make_fixture
  printf 'FROM runtime:latest # @sha256:%s\n' "$digest" >>"$scratch/Dockerfile"
  check_fixture "digest-in-comment-is-not-a-pin" 1 "FROM image must carry an @sha256: digest"
  make_fixture
  printf '%s\n' '# comment \' 'FROM runtime:latest' >>"$scratch/Dockerfile"
  check_fixture "comment-cannot-hide-unpinned-stage" 1 "FROM image must carry an @sha256: digest"
  make_fixture
  printf '%s\n' 'FROM runtime@sha256:1234' >>"$scratch/Dockerfile"
  check_fixture "malformed-digest" 1 "FROM image must carry an @sha256: digest"
  make_fixture
  printf '%s\n' '# FROM runtime:latest' >"$scratch/Dockerfile"
  check_fixture "no-stages" 1 "no FROM instructions found"
  make_fixture
  printf '%s\n' 'RUSTSEC-2000-0001' >"$scratch/SECURITY.md"
  check_fixture "second-ignore-missing-review" 1 "missing review for RUSTSEC-2000-0002"
  make_fixture
  printf '%s\n' '[advisories]' 'ignore = [{ id = "RUSTSEC-2000-0001" }]' >"$scratch/deny.toml"
  check_fixture "missing-ignore-reason" 1 "RUSTSEC-2000-0001 needs a reason"
  make_fixture
  printf '%s\n' '[advisories]' 'ignore = [' >"$scratch/deny.toml"
  check_fixture "invalid-toml" 1 "dependency-policy gate:"
  make_fixture
  printf '%s\n' '[advisories]' 'ignore = []' >"$scratch/deny.toml"
  check_fixture "no-remaining-ignores" 0 "all advisory ignores documented"

  if [ "$failures" -gt 0 ]; then
    echo "check-dependency-policy.sh --self-test: $failures scenario(s) failed" >&2
    return 1
  fi
  echo "check-dependency-policy.sh --self-test: all scenarios passed"
}

if [ "${1:-}" = "--self-test" ]; then
  run_self_test
else
  repo_root="$(git rev-parse --show-toplevel)"
  run_gate "$repo_root"
fi
