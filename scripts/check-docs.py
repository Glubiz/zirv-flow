#!/usr/bin/env python3
"""Check local Markdown links and the getting-started/flows code contracts."""

import json
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
GUIDES = ("docs/guide/getting-started.md", "docs/guide/flows.md")


def prose(text):
    """Blank fenced code without moving line numbers."""
    fence = None
    lines = []
    for line in text.splitlines(keepends=True):
        match = re.match(r"^ {0,3}(`{3,}|~{3,})", line)
        if match and fence is None:
            fence = match[1]
            lines.append("\n")
        elif fence is not None:
            if match and match[1][0] == fence[0] and len(match[1]) >= len(fence):
                fence = None
            lines.append("\n")
        else:
            lines.append(line)
    return "".join(lines)


def anchors(text):
    text = prose(text)
    found = set(re.findall(r'<[^>]+\b(?:id|name)=["\']([^"\']+)["\']', text))
    for match in re.finditer(r"^ {0,3}#{1,6}\s+(.+?)\s*#*\s*$|^([^\n]+)\n(?:=+|-+)\s*$", text, re.M):
        heading = match[1] or match[2]
        heading = re.sub(r"\[([^]]+)\]\([^)]*\)", r"\1", heading)
        heading = re.sub(r"<[^>]+>", "", heading).lower()
        slug = re.sub(r"[^\w\-\s]", "", heading).replace(" ", "-")
        candidate = slug
        suffix = 0
        while candidate in found:
            suffix += 1
            candidate = f"{slug}-{suffix}"
        found.add(candidate)
    return found


def links(text):
    text = re.sub(r"(`+)([^`\n]*?)\1", "", prose(text))
    definitions = {}
    for match in re.finditer(r"^ {0,3}\[([^]]+)\]:\s*(<[^>]+>|\S+)", text, re.M):
        definitions[match[1].casefold()] = match[2].strip("<>")
        yield match.start(), match[2].strip("<>")
    for match in re.finditer(r"!?\[[^]\n]*\]\(\s*(<[^>]+>|[^\s)]+)(?:\s+[\"'][^\n]*?[\"'])?\s*\)", text):
        yield match.start(), match[1].strip("<>")
    for match in re.finditer(r"!?\[([^]\n]+)\]\[([^]\n]*)\]", text):
        name = (match[2] or match[1]).casefold()
        yield match.start(), definitions.get(name, f"missing-reference:{name}")
    for match in re.finditer(r'<[^>]+\b(?:href|src)=["\']([^"\']+)["\']', text):
        yield match.start(), match[1]


def check_links(root, pages):
    errors = []
    for page in pages:
        text = page.read_text()
        for offset, target in links(text):
            where = f"{page.relative_to(root)}:{prose(text)[:offset].count(chr(10)) + 1}"
            if target.startswith("missing-reference:"):
                errors.append(f"{where}: undefined {target}")
                continue
            url = urlsplit(target)
            if url.scheme or url.netloc:
                continue
            path = unquote(url.path)
            destination = (root / path.lstrip("/") if path.startswith("/") else page.parent / path) if path else page
            if not destination.exists():
                errors.append(f"{where}: missing link target {target}")
            elif url.fragment and destination.suffix.lower() == ".md":
                if unquote(url.fragment) not in anchors(destination.read_text()):
                    errors.append(f"{where}: missing anchor {target}")
    return errors


def check_accuracy(text, paths, config, nodes, actions):
    errors = []
    for match in re.finditer(r"/api/v1/[A-Za-z0-9_/{}/.:-]+", text):
        route = match[0].rstrip(".")
        method = re.search(r"\b(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\s+$", text[:match.start()])
        if route not in paths:
            errors.append(f"unknown API route {route}")
        elif method and method[1].lower() not in paths[route]:
            errors.append(f"unknown API method {method[1]} {route}")
    # Full dotted inline identifiers must name a config key or a known node,
    # decision action or webhook completion event.
    for key in re.findall(r"`([a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+)`", text):
        if key not in config | nodes | actions:
            errors.append(f"unknown config key or registered type/action {key}")
    for name in set(re.findall(r"\bZIRV_FLOW_[A-Z][A-Z0-9_]*", text)):
        key = name.removeprefix("ZIRV_FLOW_").lower().replace("__", ".")
        if key not in config:
            errors.append(f"unknown configuration variable {name}")
    return errors


def check_nodes(text, nodes):
    documented = re.findall(r"^## [^\n]+ \(`([a-z][a-z0-9_.]*)`\)$", text, re.M)
    errors = [f"node type missing from headings: {node}" for node in sorted(nodes - set(documented))]
    errors += [f"unknown node type in heading: {node}" for node in sorted(set(documented) - nodes)]
    errors += [f"node type has duplicate headings: {node}" for node in sorted(set(documented)) if documented.count(node) != 1]
    return errors


def check(root):
    pages = [root / "README.md", *sorted((root / "docs").rglob("*.md"))]
    errors = check_links(root, pages)
    paths = json.loads((root / "crates/zirv-flow-api/openapi.json").read_text())["paths"]
    source = (root / "crates/zirv-flow/src/config.rs").read_text()
    keys = re.search(r"const CONFIG_KEYS:.*?= \[(.*?)\];", source, re.S)
    config = set(re.findall(r'"([a-z][a-z_.]+)"', keys[1]))
    source = (root / "crates/zirv-flow-core/src/flow/graph.rs").read_text()
    enum = source.split("pub enum NodeKind {", 1)[1].split("\nimpl TryFrom", 1)[0]
    variants = re.findall(r"^    ([A-Z]\w*)\s*[{,]", enum, re.M)
    nodes = set(re.findall(r'#\[serde\(rename = "([^"]+)"\)\]', enum))
    if not nodes or len(nodes) != len(variants):
        errors.append("cannot map every NodeKind variant to its serialized name")
    source = (root / "crates/zirv-flow-core/src/decision_actions.rs").read_text()
    actions = set(re.findall(r'^    [A-Z_0-9]+: "([^"]+)"', source, re.M))
    # Webhook completion events are emitted by the webhook worker.
    source = (root / "crates/zirv-flow-jobs/src/kinds/webhook_call.rs").read_text()
    actions |= set(re.findall(r'"(webhook\.(?:succeeded|failed))"', source))
    for guide in GUIDES:
        page = root / guide
        if not page.is_file():
            errors.append(f"missing guide {guide}")
            continue
        errors += [f"{guide}: {error}" for error in check_accuracy(page.read_text(), paths, config, nodes, actions)]
        if guide.endswith("/flows.md"):
            errors += [f"{guide}: {error}" for error in check_nodes(page.read_text(), nodes)]
    return errors, len(pages), len(nodes)


class DocumentationTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.page = self.root / "README.md"

    def fixture_repo(self):
        files = {
            "README.md": "# Guide checks\n",
            GUIDES[0]: "# Getting started\n`POST /api/v1/things/{id}`\n`server.public_url`\n",
            GUIDES[1]: "# Flows\n## Wait (`wait.duration`)\n## Exit (`exit`)\n",
            "crates/zirv-flow-api/openapi.json": json.dumps({"paths": {
                "/api/v1/things/{id}": {"post": {}}
            }}),
            "crates/zirv-flow/src/config.rs":
                'const CONFIG_KEYS: [&str; 1] = ["server.public_url"];\n',
            "crates/zirv-flow-core/src/flow/graph.rs": (
                'pub enum NodeKind {\n'
                '    #[serde(rename = "wait.duration")]\n'
                '    WaitDuration { seconds: u64 },\n'
                '    #[serde(rename = "exit")]\n'
                '    Exit {},\n}\n'
                'impl TryFrom<Value> for FlowNode {}\n'
            ),
            "crates/zirv-flow-core/src/decision_actions.rs":
                '    SEND_GATE_PASSED: "send.gate", "passed";\n',
            "crates/zirv-flow-jobs/src/kinds/webhook_call.rs":
                'let event = "webhook.succeeded";\n',
            "scripts/check-docs.py": Path(__file__).read_text(),
        }
        for name, contents in files.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents)

    def test_relative_links_and_ignored_code(self):
        (self.root / "target.md").write_text("# Target\n")
        self.page.write_text(
            '[relative](target.md) [root](/target.md) [external](https://example.com)\n'
            '`[inline example](missing.md)`\n```md\n[fenced example](missing.md)\n```\n'
        )
        self.assertEqual(check_links(self.root, [self.page]), [])
        self.page.write_text("# Start\n\n[broken](missing.md)\n")
        self.assertEqual(check_links(self.root, [self.page]), [
            "README.md:3: missing link target missing.md"
        ])

    def test_heading_duplicate_and_explicit_anchors(self):
        (self.root / "target.md").write_text(
            '# A `heading`\n\n## A heading\n\n<a id="explicit"></a>\n'
        )
        self.page.write_text(
            '[one](target.md#a-heading) [two](target.md#a-heading-1)\n'
            '[three](target.md#explicit)\n'
        )
        self.assertEqual(check_links(self.root, [self.page]), [])
        self.page.write_text("[broken](target.md#absent)\n")
        self.assertEqual(check_links(self.root, [self.page]), [
            "README.md:1: missing anchor target.md#absent"
        ])

    def test_images_and_reference_links(self):
        (self.root / "image.png").write_bytes(b"fixture")
        (self.root / "target.md").write_text("# Target\n")
        self.page.write_text(
            '![image](image.png) [reference][ref]\n[ref]: target.md\n'
        )
        self.assertEqual(check_links(self.root, [self.page]), [])
        for text, expected in (
            ("![image](absent.png)", "README.md:1: missing link target absent.png"),
            ("[ref][undefined]", "README.md:1: undefined missing-reference:undefined"),
        ):
            with self.subTest(text=text):
                self.page.write_text(text)
                self.assertEqual(check_links(self.root, [self.page]), [expected])

    def test_api_routes_and_methods(self):
        paths = {"/api/v1/things/{id}": {"post": {}}}
        self.assertEqual(check_accuracy('`POST /api/v1/things/{id}`', paths, set(), set(), set()), [])
        self.assertEqual(check_accuracy(
            '`/api/v1/missing`\n`GET /api/v1/things/{id}`', paths, set(), set(), set()
        ), [
            "unknown API route /api/v1/missing",
            "unknown API method GET /api/v1/things/{id}",
        ])

    def test_config_keys_environment_and_registered_symbols(self):
        config = {"server.public_url"}
        nodes = {"wait.duration", "exit"}
        actions = {"send.gate", "webhook.succeeded"}
        good = ('`server.public_url` `send.gate` `wait.duration` `webhook.succeeded`\n'
                'ZIRV_FLOW_SERVER__PUBLIC_URL: x')
        self.assertEqual(check_accuracy(good, {}, config, nodes, actions), [])
        self.assertEqual(check_accuracy(
            '`server.public_urll`\nZIRV_FLOW_SERVER__PUBLIC_URR', {}, config, nodes, actions
        ), [
            "unknown config key or registered type/action server.public_urll",
            "unknown configuration variable ZIRV_FLOW_SERVER__PUBLIC_URR",
        ])

    def test_every_node_has_exactly_one_heading(self):
        nodes = {"wait.duration", "exit"}
        good = '## Wait (`wait.duration`)\n\n## Exit (`exit`)\n'
        self.assertEqual(check_nodes(good, nodes), [])
        self.assertEqual(check_nodes('', nodes), [
            "node type missing from headings: exit",
            "node type missing from headings: wait.duration",
        ])
        self.assertEqual(check_nodes(good + '## Unknown (`wait.forever`)\n', nodes), [
            "unknown node type in heading: wait.forever"
        ])
        self.assertEqual(check_nodes(good + '## Exit (`exit`)\n', nodes), [
            "node type has duplicate headings: exit"
        ])

    def test_unmapped_node_variant_is_not_silently_omitted(self):
        self.fixture_repo()
        self.assertEqual(check(self.root), ([], 3, 2))
        source = self.root / "crates/zirv-flow-core/src/flow/graph.rs"
        source.write_text(source.read_text().replace('    Exit {},', '    Exit {},\n    NewNode {},'))
        self.assertEqual(check(self.root), ([
            "cannot map every NodeKind variant to its serialized name"
        ], 3, 2))

    def test_cli_summary_and_invalid_argument(self):
        self.fixture_repo()
        command = [sys.executable, "-B", str(self.root / "scripts/check-docs.py")]
        result = subprocess.run(command, capture_output=True, text=True, check=False)
        self.assertEqual((result.returncode, result.stdout, result.stderr), (
            0, "Documentation checks passed (3 Markdown pages, two guides, 2 node types).\n", ""
        ))
        result = subprocess.run(command + ["--unknown"], capture_output=True, text=True, check=False)
        self.assertEqual((result.returncode, result.stdout, result.stderr), (
            1, "", "usage: check-docs.py [--self-test]\n"
        ))

    def test_missing_guide_fails_cli_without_success_message(self):
        self.fixture_repo()
        (self.root / GUIDES[0]).unlink()
        result = subprocess.run(
            [sys.executable, "-B", str(self.root / "scripts/check-docs.py")],
            capture_output=True, text=True, check=False,
        )
        self.assertEqual((result.returncode, result.stdout, result.stderr), (
            1, "", "missing guide docs/guide/getting-started.md\n"
        ))


def self_test():
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(DocumentationTests)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    if not result.wasSuccessful():
        sys.exit(1)
    print("Documentation checker self-test passed (links, anchors, images, routes, config, nodes).")


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    elif sys.argv[1:]:
        sys.exit("usage: check-docs.py [--self-test]")
    else:
        failures, page_count, node_count = check(ROOT)
        if failures:
            print("\n".join(failures), file=sys.stderr)
            sys.exit(1)
        print(f"Documentation checks passed ({page_count} Markdown pages, two guides, {node_count} node types).")
