#!/usr/bin/env bash
# Check release evidence without building Rust or running benchmarks.
# Usage: release-check.sh [--self-test]
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/.." && pwd)"
milestone='M8 — Beta hardening and v1.0'
required_files=(
  docs/operations/secrets.md docs/operations/backup-restore.md
  docs/operations/upgrade.md docs/operations/sizing.md docs/operations/runbooks.md
  docs/operations/runtime.md docs/operations/access.md
  docs/guide/getting-started.md docs/guide/flows.md docs/guide/sources.md
  docs/guide/segments.md docs/guide/templates.md docs/guide/channels.md
  docs/guide/compliance.md docs/guide/compliance-tests.md
  docs/api.md docs/threat-model.md docs/api-audit.md docs/benchmarks/m8-validation.md
)

check_offline() {
  python3 -I - "$1" "${required_files[@]}" <<'PY'
import json, os, pathlib, re, sys

root = pathlib.Path(sys.argv[1])
errors = []
for name in sys.argv[2:]:
    if not (root / name).is_file():
        errors.append(f'missing required file {name}')

api = 'crates/zirv-flow-api/openapi.json'
try:
    value = json.loads((root / api).read_text())
    if value.get('info', {}).get('version') != '1':
        errors.append(f'{api}: info.version must be "1"')
except (OSError, ValueError) as error:
    errors.append(f'{api}: {error}')

dockerfiles = []
for directory, dirs, files in os.walk(root):
    dirs[:] = [name for name in dirs if name not in {
        '.git', '.zirv', '.claude', 'node_modules', 'target', '.venv'
    }]
    dockerfiles += [pathlib.Path(directory) / name for name in files
                    if name.lower().startswith('dockerfile') or name.lower().endswith('.dockerfile')]
if not dockerfiles:
    errors.append('no Dockerfiles found')
for path in sorted(dockerfiles):
    name = path.relative_to(root)
    source = re.sub(r'\\\r?\n', ' ', path.read_text())
    for number, line in enumerate(source.splitlines(), 1):
        if not re.match(r'^\s*FROM(?:\s|$)', line, re.I):
            continue
        if not re.fullmatch(
            r'\s*FROM\s+(?:--platform=\S+\s+)?[^\s@]+@sha256:[a-fA-F0-9]{64}'
            r'(?:\s+AS\s+\S+)?\s*', line, re.I
        ):
            errors.append(f'{name}:{number}: FROM must pin an image by sha256 digest: {line.strip()}')

for error in errors:
    print('release-check: ' + error, file=sys.stderr)
sys.exit(1 if errors else 0)
PY
}

check_issues() {
  local label="$1" issues
  issues="$(gh issue list -R Glubiz/zirv-flow --milestone "$milestone" \
    --label "$label" --state open --limit 1000 --json number,title \
    --jq '.[] | "#\(.number) \(.title)"')" || { echo "could not query $label issues in $milestone" >&2; return 1; }
  [ -z "$issues" ] || { echo "open $label issues in $milestone:" >&2; printf '%s\n' "$issues" >&2; return 1; }
}

run_all() {
  local tree="$1" failures=0 checks=0
  run() {
    local name="$1" status=0
    shift
    checks=$((checks + 1))
    "$@" || status=$?
    if [ "$status" -eq 0 ]; then
      echo "PASS: $name"
    else
      echo "FAIL: $name (exit $status)" >&2
      failures=$((failures + 1))
    fi
  }
  run 'M8 severity:blocker issues' check_issues severity:blocker
  run 'M8 severity:major issues' check_issues severity:major
  run 'required files, API version and Dockerfile digests' check_offline "$tree"
  run 'shipped build inputs' bash "$tree/scripts/check-bench-features.sh" --static-only "$tree"
  run 'M8 report' bash "$tree/scripts/bench/m8/check-report.sh"
  run 'documentation' python3 "$tree/scripts/check-docs.py"
  echo "release-check: $failures of $checks checks failed"
  [ "$failures" -eq 0 ]
}

run_self_test() {
  work="$(mktemp -d "${TMPDIR:-/tmp}/release-check.XXXXXX")"
  trap 'rm -rf "$work"' EXIT
  python3 -I - "$work" "${required_files[@]}" <<'PY'
import json, pathlib, shutil, sys

work = pathlib.Path(sys.argv[1])
base = work / 'complete'
for name in sys.argv[2:]:
    path = base / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text('# Fixture\n')
api = pathlib.Path('crates/zirv-flow-api/openapi.json')
(base / api).parent.mkdir(parents=True)
(base / api).write_text(json.dumps({'info': {'version': '1'}}))
digest = 'a' * 64
(base / 'Dockerfile').write_text(
    '# FROM ignored\nFROM --platform=$BUILDPLATFORM image@sha256:' + digest + ' AS builder\n'
    'from runtime@sha256:' + digest + '\n')
for name, content in {
    'scripts/check-bench-features.sh': '[[ "$1" = --static-only ]] || exit 2\necho fixture-bench\n',
    'scripts/bench/m8/check-report.sh': 'echo fixture-report\n',
    'scripts/check-docs.py': 'print("fixture-docs")\n',
}.items():
    path = base / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)

def fixture(name):
    return pathlib.Path(shutil.copytree(base, work / name))

(fixture('missing-files') / 'docs/operations/sizing.md').unlink()
(work / 'missing-files/docs/guide/flows.md').unlink()
(fixture('missing-api') / api).unlink()
for name, value in (('old-version', '0.194.0'), ('numeric-version', 1)):
    (fixture(name) / api).write_text(json.dumps({'info': {'version': value}}))
(fixture('invalid-json') / api).write_text('{')
(fixture('short-digest') / 'Dockerfile').write_text('FROM image@sha256:abc\n')
(fixture('missing-dockerfile') / 'Dockerfile').unlink()
nested = fixture('nested-unpinned') / 'deploy/test.Dockerfile'
nested.parent.mkdir()
nested.write_text('FROM image:latest\n')
continued = fixture('continued-from') / 'Dockerfile'
continued.write_text('FROM --platform=$BUILDPLATFORM \\\n image@sha256:' + digest + ' AS builder\n')
multiple = fixture('multiple-failures')
(multiple / 'docs/operations/sizing.md').unlink()
(multiple / api).write_text('{')
(multiple / 'Dockerfile').write_text('FROM image:latest\n')
(multiple / 'scripts/check-bench-features.sh').write_text('echo fixture-bench-failed\nexit 7\n')
(multiple / 'scripts/bench/m8/check-report.sh').write_text('echo fixture-report-failed\nexit 8\n')
(multiple / 'scripts/check-docs.py').write_text('print("fixture-docs-failed")\nraise SystemExit(9)\n')
(work / 'bin').mkdir()
gh = work / 'bin/gh'
gh.write_text('''#!/usr/bin/env bash
case "$RELEASE_TEST_ISSUES" in
  clear) exit 0 ;;
  open) echo '#123 fixture issue' ;;
  error) echo 'fixture gh unavailable' >&2; exit 1 ;;
esac
''')
gh.chmod(0o755)
PY
  local failures=0
  expect() {
    local name="$1" wanted="$2" message="$3" got=0
    shift 3
    "$@" >"$work/output" 2>&1 || got=$?
    if [ "$got" -eq "$wanted" ] && [[ "$(<"$work/output")" = *"$message"* ]]; then
      echo "self-test ok: $name (exit $got)"
    else
      echo "self-test FAILED: $name (expected exit $wanted, got $got; diagnostic: $message)" >&2
      cat "$work/output" >&2
      failures=$((failures + 1))
    fi
  }
  expect complete 0 '' check_offline "$work/complete"
  expect missing-files 1 'missing required file docs/operations/sizing.md' check_offline "$work/missing-files"
  expect missing-api 1 'openapi.json:' check_offline "$work/missing-api"
  for name in old-version numeric-version; do
    expect "$name" 1 'info.version must be "1"' check_offline "$work/$name"
  done
  expect invalid-json 1 'openapi.json:' check_offline "$work/invalid-json"
  expect short-digest 1 'FROM must pin an image by sha256 digest' check_offline "$work/short-digest"
  expect missing-dockerfile 1 'no Dockerfiles found' check_offline "$work/missing-dockerfile"
  expect nested-unpinned 1 'deploy/test.Dockerfile:' check_offline "$work/nested-unpinned"
  expect continued-from 0 '' check_offline "$work/continued-from"
  PATH="$work/bin:$PATH" RELEASE_TEST_ISSUES=clear \
    expect all-pass 0 'release-check: 0 of 6 checks failed' run_all "$work/complete"
  PATH="$work/bin:$PATH" RELEASE_TEST_ISSUES=open \
    expect open-issues 1 '#123 fixture issue' run_all "$work/complete"
  PATH="$work/bin:$PATH" RELEASE_TEST_ISSUES=error \
    expect query-error 1 'could not query severity:major issues' run_all "$work/complete"
  PATH="$work/bin:$PATH" RELEASE_TEST_ISSUES=error \
    expect all-failures 1 'release-check: 6 of 6 checks failed' run_all "$work/multiple-failures"
  for message in 'missing required file' 'openapi.json:' 'FROM must pin' \
    'shipped build inputs (exit 7)' 'M8 report (exit 8)' 'documentation (exit 9)'; do
    if ! grep -qF "$message" "$work/output"; then
      echo "self-test FAILED: missing diagnostic: $message" >&2
      failures=$((failures + 1))
    fi
  done
  [ "$failures" -eq 0 ] || return 1
  echo 'release-check.sh --self-test: all scenarios passed'
}

usage() { echo "usage: $0 [--self-test]" >&2; exit 2; }
[ "$#" -le 1 ] || usage
case "${1:-}" in
  --self-test) run_self_test ;;
  '') run_all "$root" ;;
  *) usage ;;
esac
