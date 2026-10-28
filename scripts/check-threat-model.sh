#!/usr/bin/env bash
# Checks the threat model's named tests and architecture trust-boundary coverage.
# Usage: check-threat-model.sh [--self-test]
# Requires Python 3; performs no builds or network requests.

set -euo pipefail

if [ "$#" -gt 1 ] || { [ "$#" -eq 1 ] && [ "$1" != "--self-test" ]; }; then
  echo "usage: $0 [--self-test]" >&2
  exit 2
fi

root="$(git rev-parse --show-toplevel)"
python3 - "$root" "${1:-}" <<'PY'
from pathlib import Path
import re
import sys
import tempfile

TEST_LINK = re.compile(r'\[test: `([^`\n]+)`\]\(([^)\n]+)\)')
SOURCE = re.compile(r'\.\./((?:crates|web)/[^#]+\.(?:rs|ts|tsx))#L([1-9][0-9]*)')


def boundaries(architecture):
    section = re.search(r'^### 16\.1 Trust boundaries\s*\n(.*?)(?=^### |\Z)',
                        architecture, re.M | re.S)
    if not section:
        raise ValueError('architecture 16.1 is missing')
    rows = [line.split('|')[1].strip() for line in section[1].splitlines()
            if line.startswith('|')]
    return [row for row in rows if row != 'Boundary'
            and not re.fullmatch(r'[- :]+', row)]


def is_test(source, name, suffix):
    if suffix == '.rs':
        # Include proptest declarations; a helper with the same name is not evidence.
        pattern = (r'^\s*#\[(?:test|tokio::test|sqlx::test)(?:\([^\]]*\))?\]\s*'
                   r'(?:#\[[^\n]*\]\s*|//[^\n]*\n\s*)*'
                   r'(?:async\s+)?fn\s+' + re.escape(name) + r'\s*\(')
    else:
        pattern = (r'^\s*(?:test|it)\(\s*([\'"])' + re.escape(name)
                   + r'\1\s*,')
    return re.search(pattern, source, re.M) is not None


def check(root):
    model = (root / 'docs/threat-model.md').read_text()
    required = boundaries((root / 'docs/architecture.md').read_text())
    errors = []
    if not required:
        errors.append('architecture 16.1 has no boundary rows')
    sections = re.findall(r'^### ([^\n]+)\n(.*?)(?=^#{1,3} |\Z)', model, re.M | re.S)
    for boundary in required:
        found = [body for title, body in sections if title == boundary]
        if len(found) != 1:
            errors.append(f'boundary needs exactly one section: {boundary}')
        elif not TEST_LINK.search(found[0]) and not re.search(r'tracked in #(?:TBD-)?[1-9][0-9]*\b', found[0]):
            errors.append(f'boundary has neither a named test nor a tracked gap: {boundary}')

    tests = TEST_LINK.findall(model)
    if not tests:
        errors.append('no named tests found')
    if model.count('[test:') != len(tests):
        errors.append('malformed test citation; use [test: `name`](../path#Lline)')
    for name, target in tests:
        match = SOURCE.fullmatch(target)
        if not match:
            errors.append(f'{name}: expected a source path and line, got {target}')
            continue
        path = (root / match[1]).resolve()
        if root.resolve() not in path.parents or not path.is_file():
            errors.append(f'{name}: source is missing or outside the repository: {target}')
        elif not is_test(path.read_text(), name, path.suffix):
            errors.append(f'{match[1]}: no test named {name}')
    return errors, len(required), len(tests)


def self_test():
    with tempfile.TemporaryDirectory(prefix='threat-model-') as directory:
        root = Path(directory)
        (root / 'docs').mkdir()
        (root / 'crates/example/src').mkdir(parents=True)
        (root / 'web/e2e').mkdir(parents=True)
        architecture = '### 16.1 Trust boundaries\n\n| Boundary | Threat | Control |\n|---|---|---|\n| Browser to `serve` | XSS | CSP |\n\n### 16.2 Secrets\n'
        model = '### Browser to `serve`\n\n[test: `cookie`](../crates/example/src/lib.rs#L2)\n[test: `preview blocks scripts`](../web/e2e/preview.spec.ts#L1)\n'
        rust = '#[sqlx::test]\n#[ignore = "database"]\nasync fn cookie() {}\n'
        web = 'test("preview blocks scripts", async () => {});\n'
        files = {
            'docs/architecture.md': architecture,
            'docs/threat-model.md': model,
            'crates/example/src/lib.rs': rust,
            'web/e2e/preview.spec.ts': web,
        }

        def scenario(name, changes, failure=None):
            for file, text in {**files, **changes}.items():
                (root / file).write_text(text)
            errors, _, _ = check(root)
            if failure is None:
                assert not errors, (name, errors)
            else:
                assert any(failure in error for error in errors), (name, errors)

        scenario('valid references', {})
        scenario('renamed Rust test', {'crates/example/src/lib.rs': rust.replace('cookie', 'renamed')}, 'no test named cookie')
        scenario('helper is not a test', {'crates/example/src/lib.rs': 'fn cookie() {}\n'}, 'no test named cookie')
        scenario('renamed browser test', {'web/e2e/preview.spec.ts': web.replace('blocks', 'permits')}, 'no test named preview blocks scripts')
        scenario('missing source', {'docs/threat-model.md': model.replace('lib.rs', 'gone.rs')}, 'source is missing')
        scenario('missing boundary', {'docs/threat-model.md': model.replace('### Browser to `serve`', '### Renamed')}, 'boundary needs exactly one section')
        scenario('new boundary', {'docs/architecture.md': architecture.replace('\n### 16.2', '\n| New boundary | Threat | Control |\n\n### 16.2')}, 'New boundary')
        scenario('duplicate boundary', {'docs/threat-model.md': model + '\n### Browser to `serve`\n'}, 'boundary needs exactly one section')
        scenario('empty boundary', {'docs/threat-model.md': '### Browser to `serve`\n\n## Evidence elsewhere\n' + model.split('\n\n', 1)[1]}, 'neither a named test nor a tracked gap')
        scenario('tracked gap', {'docs/threat-model.md': '### Browser to `serve`\n\nControl has no test; tracked in #TBD-1.\n\n## Evidence elsewhere\n' + model.split('\n\n', 1)[1]})
        scenario('malformed citation', {'docs/threat-model.md': model.replace('`cookie`', 'cookie')}, 'malformed test citation')
        scenario('missing line', {'docs/threat-model.md': model.replace('#L2', '')}, 'expected a source path and line')
        scenario('line movement preserves historical citation', {'crates/example/src/lib.rs': '\n\n' + rust})
        scenario('proptest', {'crates/example/src/lib.rs': 'proptest! {\n    #[test]\n    fn cookie(value in inputs()) {}\n}\n'})
        scenario('test attribute arguments', {'crates/example/src/lib.rs': rust.replace('sqlx::test', 'tokio::test(flavor = "multi_thread")')})
        scenario('unpadded boundary row', {'docs/architecture.md': architecture.replace('\n### 16.2', '\n|New boundary|Threat|Control|\n\n### 16.2')}, 'New boundary')
    print('check-threat-model.sh --self-test: all 16 scenarios passed')


try:
    if sys.argv[2] == '--self-test':
        self_test()
    else:
        errors, boundary_count, test_count = check(Path(sys.argv[1]))
        if errors:
            for error in errors:
                print(f'threat-model gate: {error}', file=sys.stderr)
            sys.exit(1)
        print(f'threat-model gate: {boundary_count} boundaries; {test_count} test citations verified')
except (OSError, ValueError, AssertionError) as error:
    print(f'threat-model gate: {error}', file=sys.stderr)
    sys.exit(1)
PY
