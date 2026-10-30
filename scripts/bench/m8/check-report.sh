#!/usr/bin/env bash
# Check the section 19 report and recursive M8 data, without running benchmarks.
# Usage: check-report.sh [--self-test | --data-only]
# Exit 1 on invalid input, 2 on usage errors. Requires bash and Python 3.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../../.." && pwd)"

run_check() {
  python3 -I - "$1" "$2" <<'PY'
import json, pathlib, re, sys
from urllib.parse import unquote, urlsplit

root = pathlib.Path(sys.argv[1]).resolve()
data_dir = (root / 'docs/benchmarks/data/m8').resolve()
report_path = root / 'docs/benchmarks/m8-validation.md'
label = 'reference-resource emulation, not a reference-hardware run'
verdicts = {'met', 'missed and fixed', 'revised by decision'}


def prose(text):
    text = re.sub(r'<!--.*?-->', '', text, flags=re.S)
    lines, fence = [], None
    for line in text.splitlines():
        match = re.match(r'^ {0,3}(`{3,}|~{3,})', line)
        if fence is None and match:
            fence = match[1]
        elif fence is not None:
            if match and match[1][0] == fence[0] and len(match[1]) >= len(fence):
                fence = None
        else:
            lines.append(line)
            continue
        lines.append('')
    return '\n'.join(lines)


def cells(line):
    line = line.strip()
    if not line.startswith('|') or not line.endswith('|'):
        return []
    return [' '.join(cell.replace(r'\|', '|').split())
            for cell in re.split(r'(?<!\\)\|', line[1:-1])]


def table(text, where):
    lines = text.splitlines()
    starts = [i for i, line in enumerate(lines) if 'Dimension' in cells(line)]
    if not starts:
        raise ValueError(f'{where}: missing Dimension table')
    if len(starts) != 1:
        raise ValueError(f'{where}: expected one Dimension table')
    start = starts[0]
    header = cells(lines[start])
    separator = cells(lines[start + 1]) if start + 1 < len(lines) else []
    if (len(set(header)) != len(header) or len(separator) != len(header)
            or not all(re.fullmatch(r':?-{3,}:?', cell) for cell in separator)):
        raise ValueError(f'{where}: malformed Dimension table header')
    rows = []
    for number in range(start + 2, len(lines)):
        line = lines[number]
        if not line.strip().startswith('|'):
            break
        row = cells(line)
        if len(row) != len(header):
            raise ValueError(f'{where}: malformed table row: {line}')
        rows.append(dict(zip(header, row)))
    if not rows:
        raise ValueError(f'{where}: empty Dimension table')
    return header, rows


def dimensions():
    text = prose((root / 'docs/architecture.md').read_text(encoding='utf-8'))
    section = re.search(r'^## 19\. [^\n]*(?:\n|\Z)(.*?)(?=^## |\Z)', text, re.M | re.S)
    if section is None:
        raise ValueError('docs/architecture.md: missing section 19')
    _, rows = table(section[1], 'docs/architecture.md section 19')
    names = [row['Dimension'] for row in rows]
    if any(not name for name in names) or len(set(names)) != len(names):
        raise ValueError('docs/architecture.md section 19: empty or duplicate dimension')
    return names


def check_citations(cell, where):
    # Links resolve from the report; literal repository paths are accepted as well.
    paths = re.findall(r'\[[^\]]*\]\(([^\s)]+)\)', cell) or [cell.strip('`')]
    for target in paths:
        url = urlsplit(target.strip('<>'))
        path = unquote(url.path)
        base = root if path.startswith(('docs/', '/')) else report_path.parent
        resolved = (base / path.lstrip('/')).resolve()
        if (url.scheme or url.netloc or not resolved.is_relative_to(data_dir)
                or resolved.suffix != '.json'):
            raise ValueError(f'{where}: citation must be a JSON file under docs/benchmarks/data/m8/: {target}')
        if not resolved.is_file():
            raise ValueError(f'{where}: missing data file: {target}')


def reject_constant(value):
    raise ValueError(f'non-JSON constant {value}')


def check():
    files = sorted(data_dir.rglob('*.json'))
    for path in files:
        where = path.relative_to(root)
        try:
            value = json.loads(path.read_text(encoding='utf-8'), parse_constant=reject_constant)
        except (ValueError, OSError) as error:
            raise ValueError(f'{where}: invalid JSON: {error}') from error
        if not isinstance(value, dict):
            raise ValueError(f'{where}: expected a JSON object')
        if not isinstance(value.get('image_version'), str) or not value['image_version'].strip():
            raise ValueError(f'{where}: image_version must be a nonempty string')
        if not isinstance(value.get('topology'), dict):
            raise ValueError(f'{where}: topology must be an object')
    print(f'm8 data: {len(files)} JSON file(s) checked')
    if sys.argv[2] == 'data-only':
        return
    names = dimensions()
    if not report_path.exists():
        print(f'm8 report: absent; skipped ({len(names)} section 19 dimensions)')
        return
    report = prose(report_path.read_text(encoding='utf-8'))
    if label not in report:
        raise ValueError('m8-validation.md: missing emulation label: ' + label)
    header, rows = table(report, 'm8-validation.md')
    for column in ('Data file', 'Verdict'):
        if column not in header:
            raise ValueError(f'm8-validation.md: missing table column: {column}')
    seen = set()
    for row in rows:
        name = row['Dimension']
        where = f'm8-validation.md: {name}'
        if name not in names:
            raise ValueError(f'{where}: unknown dimension')
        if name in seen:
            raise ValueError(f'{where}: duplicate dimension')
        seen.add(name)
        if row['Verdict'] not in verdicts:
            raise ValueError(f"{where}: invalid verdict {row['Verdict']!r}; use met, missed and fixed, or revised by decision")
        check_citations(row['Data file'], where)
    for name in names:
        if name not in seen:
            raise ValueError(f'm8-validation.md: missing dimension: {name}')
    print(f'm8 report: {len(names)} section 19 dimensions checked')


try:
    check()
except (ValueError, OSError) as error:
    print(f'm8 check-report: {error}', file=sys.stderr)
    sys.exit(1)
PY
}

run_self_test() {
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT
  python3 -I - "$work" <<'PY'
import json, pathlib, shutil, sys

work = pathlib.Path(sys.argv[1])
base = work / 'complete-report'
data = base / 'docs/benchmarks/data/m8'
data.mkdir(parents=True)
architecture = '''## 18. Tests
| Dimension | Target |
| --- | --- |
| Outside section 19 | 1 |

## 19. Scalability envelope
| Dimension | v1 TARGET |
| --- | --- |
| Fixture people | 10 |
| Fixture ingest | 20 |
| Fixture render | 30 |

## 20. Decisions
| Dimension | Target |
| --- | --- |
| Also outside section 19 | 2 |
'''
report = '''# M8 validation
reference-resource emulation, not a reference-hardware run

| Dimension | Target | Measured | Scale | Data file | Verdict |
| --- | --- | --- | --- | --- | --- |
| Fixture people | 10 | 11 | 10 | [people](data/m8/people.json) | met |
| Fixture ingest | 20 | 21 | 20 | `docs/benchmarks/data/m8/ingest.failed.json` | missed and fixed |
| Fixture render | 30 | 31 | 30 | [render](data/m8/smoke/render.json) | revised by decision |
'''
arch_path = pathlib.Path('docs/architecture.md')
report_path = pathlib.Path('docs/benchmarks/m8-validation.md')
data_path = pathlib.Path('docs/benchmarks/data/m8')
(base / arch_path).write_text(architecture)
(base / report_path).write_text(report)
for name in ('people.json', 'ingest.failed.json', 'smoke/render.json', 'smoke/render.failed.json'):
    path = data / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(dict(image_version='0.190.0', topology=dict(containers=[]),
                                   smoke=name.startswith('smoke/'), exit_code=1 if '.failed.' in name else 0)))

def fixture(name):
    return pathlib.Path(shutil.copytree(base, work / name))

def edit_report(name, text):
    (fixture(name) / report_path).write_text(text)

empty = fixture('empty-tree')
(empty / report_path).unlink()
shutil.rmtree(empty / data_path)
edit_report('missing-dimension', '\n'.join(line for line in report.splitlines()
                                         if not line.startswith('| Fixture people')) + '\nFixture people\n')
edit_report('missing-verdict', report.replace('| met |', '| |'))
edit_report('bare-miss', report.replace('| met |', '| missed |'))
edit_report('missing-data', report.replace('data/m8/people.json', 'data/m8/absent.json'))
edit_report('outside-data', report.replace('data/m8/people.json', 'data/m8/../outside.json'))
(work / 'outside-data/docs/benchmarks/data/outside.json').write_text('{}')
edit_report('missing-label', report.replace('reference-resource emulation, not a reference-hardware run', ''))
edit_report('duplicate-dimension', report + next(line for line in report.splitlines()
                                               if line.startswith('| Fixture people')) + '\n')
for field, name in (('image_version', 'ingest.failed.json'), ('topology', 'smoke/render.failed.json')):
    path = fixture('missing-' + field) / data_path / name
    value = json.loads(path.read_text())
    del value[field]
    path.write_text(json.dumps(value))
(fixture('malformed-json') / data_path / 'smoke/render.json').write_text('{')
no_report = fixture('invalid-data-without-report')
(no_report / report_path).unlink()
(no_report / data_path / 'people.json').write_text('{')
(fixture('new-dimension') / arch_path).write_text(architecture.replace(
    '| Fixture render | 30 |', '| Fixture render | 30 |\n| New target | 40 |'))
(fixture('missing-architecture-table') / arch_path).write_text('## 19. Scalability envelope\n')
edit_report('data-only', 'unfinished report')
PY
  failures=0
  expect() {
    local name="$1" wanted="$2" message="$3" got=0
    run_check "$work/$name" "${4:-all}" >"$work/output" 2>&1 || got=$?
    if [ "$got" -eq "$wanted" ] && [[ "$(<"$work/output")" = *"$message"* ]]; then
      echo "self-test ok: $name (expected exit $wanted, actual $got)"
    else
      echo "self-test FAILED: $name (expected exit $wanted, actual $got; diagnostic: $message)" >&2
      cat "$work/output" >&2
      failures=$((failures + 1))
    fi
  }
  expect complete-report 0 ''
  expect empty-tree 0 ''
  expect missing-dimension 1 'missing dimension: Fixture people'
  expect missing-verdict 1 'invalid verdict'
  expect bare-miss 1 'invalid verdict'
  expect missing-data 1 'missing data file'
  expect outside-data 1 'must be a JSON file under docs/benchmarks/data/m8/'
  expect missing-image_version 1 'image_version must be a nonempty string'
  expect missing-topology 1 'topology must be an object'
  expect malformed-json 1 'invalid JSON'
  expect invalid-data-without-report 1 'invalid JSON'
  expect missing-label 1 'missing emulation label'
  expect duplicate-dimension 1 'duplicate dimension'
  expect new-dimension 1 'missing dimension: New target'
  expect missing-architecture-table 1 'missing Dimension table'
  expect data-only 0 '' data-only
  [ "$failures" -eq 0 ] || return 1
  echo 'check-report.sh --self-test: all scenarios passed'
}

[ "$#" -le 1 ] || { echo "usage: $0 [--self-test | --data-only]" >&2; exit 2; }
case "${1:-}" in
  --self-test) run_self_test ;;
  --data-only) run_check "$root" data-only ;;
  '') run_check "$root" all ;;
  *) echo "usage: $0 [--self-test | --data-only]" >&2; exit 2 ;;
esac
