#!/usr/bin/env python3
"""Check documentation links, guide contracts and the lean README."""

import json
import os
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
GUIDES = (
    "docs/guide/getting-started.md", "docs/guide/flows.md",
    "docs/guide/sources.md", "docs/guide/segments.md",
    "docs/guide/templates.md", "docs/guide/channels.md",
)
SIZING_GUIDE = "docs/operations/sizing.md"
README_LINE_BUDGET = 350
MOVED_HEADINGS = (
    "Master key",
    "Secrets",
    "Workspace credentials",
    "Egress",
    "Logs and traces",
    "Tracing export",
    "Metrics",
    "HTTP server",
    "Background jobs",
    "Accounts, workspaces and authentication",
    "Sign in with Keycloak",
    "People and fields",
    "Events",
    "Retention windows",
    "Event debugger and explorer",
    "Consent and suppressions",
    "Lists",
    "Segments",
    "Contacts",
    "Deletion, erasure and export",
    "Ingestion",
    "People",
    "CSV import",
    "AI assistant",
    "Mapping and manifest assistant",
    "Segment assistant",
    "Sources",
    "The file source",
    "Mapping reference",
    "Sync runs",
    "The HTTP manifest source",
    "Database sources",
    "Kafka sources",
    "Source freshness",
    "Inbound webhooks",
    "Connector runtime",
    "Tracking and unsubscribe links",
    "Automatic UTM parameters",
    "Template editor and delivery counts",
    "Template test sends",
    "Sending domains",
    "Email provider destinations",
    "Event forwarding",
    "Render host",
)


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


def repository_target(target):
    """Resolve this repository's public main-branch links against the checkout."""
    url = urlsplit(target)
    base = "/glubiz/zirv-flow"
    if url.netloc.lower() != "github.com":
        return target
    path = url.path.rstrip("/")
    if path.lower() == base:
        path = "/README.md"
    elif path.lower().startswith(base + "/blob/main/"):
        path = "/" + path[len(base + "/blob/main/"):]
    else:
        return target
    return path + ("#" + url.fragment if url.fragment else "")


def check_links(root, pages):
    errors = []
    for page in pages:
        text = page.read_text()
        targets = list(links(text)) if page.suffix == ".md" else [
            (match.start(), match[0]) for match in re.finditer(
                r"https://github\.com/[Gg]lubiz/zirv-flow[^\s\"'`<>)]*", text
            )
        ]
        for offset, target in targets:
            target = repository_target(target)
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


def check_readme(text, reports):
    errors = []
    count = len(text.splitlines())
    if count > README_LINE_BUDGET:
        errors.append(f"README.md: {count} lines exceeds budget {README_LINE_BUDGET}")
    # Explicit compatibility ids are welcome; only actual headings are forbidden.
    without_ids = re.sub(r"<[^>]+>", "", prose(text))
    headings = anchors(without_ids)
    for heading in MOVED_HEADINGS:
        if anchors("# " + heading) & headings:
            errors.append(f"README.md: moved heading reappeared: {heading}")
    linked = {str(Path(unquote(urlsplit(target).path).lstrip("/"))) for _, target in links(text)
              if not urlsplit(target).scheme and not urlsplit(target).netloc}
    for report in sorted(reports):
        if report not in linked:
            errors.append(f"README.md: benchmark report is not linked: {report}")
    return errors


def code_symbols(root, config):
    """Dotted event, audit and job names, and metric names present in Rust literals."""
    symbols, metrics, registered = set(), set(), set()
    for path in (root / "crates").rglob("*.rs"):
        source = path.read_text()
        symbols.update(re.findall(r'"([a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)+)"', source))
        metrics.update(re.findall(r'(?:counter|gauge|histogram)!\(\s*"(zirv_flow_[a-z0-9_]+)"', source))
        registered.update(re.findall(r'\baudited\("([^"]+)"\)', source))
        registered.update(re.findall(r'const [A-Z_]*KIND: [^=]+?=\s*"([^"]+)"', source))
    namespaces = {key.split(".")[0] for key in config}
    symbols = {name for name in symbols if name.split(".")[0] not in namespaces}
    return symbols | registered, metrics


def check_metrics(text, metrics):
    return [f"unknown metric {name}" for name in sorted(
        set(re.findall(r"\bzirv_flow_[a-z0-9_]+", text)) - metrics
    )]


def check_benchmark_paths(root, page, text):
    literals = {match.rstrip(".") for match in re.findall(
        r"\bdocs/benchmarks/data/[A-Za-z0-9_./-]+", text
    )}
    linked = set()
    for _, target in links(text):
        path = unquote(urlsplit(repository_target(target)).path)
        if path and not urlsplit(target).scheme:
            linked.add(os.path.normpath(root / path.lstrip("/") if path.startswith("/") else page.parent / path))
    linked = {os.path.relpath(path, root) for path in linked}
    errors = [f"missing benchmark data path {path}" for path in sorted(literals)
              if not (root / path).exists()]
    errors += [f"benchmark data must not cite smoke or failed runs: {path}"
               for path in sorted(literals | linked)
               if path.startswith("docs/benchmarks/data/")
               and ("/smoke/" in path or path.endswith(".failed.json"))]
    return errors


def check_nodes(text, nodes):
    documented = re.findall(r"^## [^\n]+ \(`([a-z][a-z0-9_.]*)`\)$", text, re.M)
    errors = [f"node type missing from headings: {node}" for node in sorted(nodes - set(documented))]
    errors += [f"unknown node type in heading: {node}" for node in sorted(set(documented) - nodes)]
    errors += [f"node type has duplicate headings: {node}" for node in sorted(set(documented)) if documented.count(node) != 1]
    return errors


def check(root):
    pages = [root / "README.md", *sorted((root / "docs").rglob("*.md"))]
    pages += [root / name for name in ("CONTRIBUTING.md", "SECURITY.md") if (root / name).is_file()]
    errors = check_links(root, pages)
    references = [path for directory in ("web/src", "crates", "scripts")
                  for path in (root / directory).rglob("*")
                  if path.suffix in {".ts", ".tsx", ".rs", ".sh", ".py"}]
    errors += check_links(root, references)
    reports = [str(path.relative_to(root)) for path in (root / "docs/benchmarks").glob("*.md")]
    errors += check_readme((root / "README.md").read_text(), reports)
    paths = json.loads((root / "crates/zirv-flow-api/openapi.json").read_text())["paths"]
    source = (root / "crates/zirv-flow/src/config.rs").read_text()
    keys = re.search(r"const CONFIG_KEYS:.*?= \[(.*?)\];", source, re.S)
    config = set(re.findall(r'"([a-z][a-z_.]+)"', keys[1]))
    symbols, metrics = code_symbols(root, config)
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
    for guide in (*GUIDES, SIZING_GUIDE):
        page = root / guide
        if not page.is_file():
            errors.append(f"missing guide {guide}")
            continue
        errors += [f"{guide}: {error}" for error in check_accuracy(page.read_text(), paths, config, nodes, actions | symbols)]
        errors += [f"{guide}: {error}" for error in check_metrics(page.read_text(), metrics)]
        if guide == SIZING_GUIDE:
            errors += [f"{guide}: {error}" for error in check_benchmark_paths(root, page, page.read_text())]
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
            "README.md": '# Guide checks\n<a id="sources"></a>\n',
            GUIDES[0]: "# Getting started\n`POST /api/v1/things/{id}`\n`server.public_url`\n",
            GUIDES[1]: "# Flows\n## Wait (`wait.duration`)\n## Exit (`exit`)\n",
            SIZING_GUIDE: "# Sizing\n",
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
        files.update({guide: "# Guide\n## Database sources\n" for guide in GUIDES[2:]})
        for name, contents in files.items():
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents)

    def test_readme_line_budget(self):
        self.assertEqual(check_readme("line\n" * README_LINE_BUDGET, []), [])
        self.assertEqual(check_readme("line\n" * (README_LINE_BUDGET + 1), []), [
            f"README.md: {README_LINE_BUDGET + 1} lines exceeds budget {README_LINE_BUDGET}"
        ])

    def test_moved_headings_cannot_return(self):
        for heading in MOVED_HEADINGS:
            with self.subTest(heading=heading):
                self.assertEqual(check_readme(f"### {heading}\n", []), [
                    f"README.md: moved heading reappeared: {heading}"
                ])
        self.assertEqual(check_readme(
            '<a id="sources"></a>[Sources](docs/guide/sources.md)\n'
            '```md\n## Sources\n```\n', []
        ), [])

    def test_every_existing_benchmark_needs_a_link(self):
        reports = ["docs/benchmarks/m2-ingestion.md", "docs/benchmarks/m8-validation.md"]
        good = "[M2](docs/benchmarks/m2-ingestion.md)\n[M8](./docs/benchmarks/m8-validation.md)\n"
        self.assertEqual(check_readme(good, reports), [])
        self.assertEqual(check_readme(
            "[M2](docs/benchmarks/m2-ingestion.md)\n`[M8](docs/benchmarks/m8-validation.md)`", reports
        ), ["README.md: benchmark report is not linked: docs/benchmarks/m8-validation.md"])
        self.assertEqual(check_readme("", []), [])

    def test_new_benchmark_report_is_discovered(self):
        self.fixture_repo()
        report = self.root / "docs/benchmarks/m8-validation.md"
        report.parent.mkdir(parents=True)
        report.write_text("# Validation\n")
        errors, _, _ = check(self.root)
        self.assertEqual(errors, [
            "README.md: benchmark report is not linked: docs/benchmarks/m8-validation.md"
        ])
        self.page.write_text(self.page.read_text() + "[Validation](/docs/benchmarks/m8-validation.md)\n")
        self.assertEqual(check(self.root)[0], [])

    def test_stale_code_literal_cannot_validate_a_removed_config_key(self):
        self.fixture_repo()
        config = self.root / "crates/zirv-flow/src/config.rs"
        config.write_text(config.read_text() + 'const OLD: &str = "server.removed";\n')
        page = self.root / GUIDES[2]
        page.write_text(page.read_text() + "`server.removed`\n")
        self.assertEqual(check(self.root)[0], [
            f"{GUIDES[2]}: unknown config key or registered type/action server.removed"
        ])

    def test_metric_must_be_emitted(self):
        self.assertEqual(check_metrics("`zirv_flow_jobs{kind,state}`", {"zirv_flow_jobs"}), [])
        self.assertEqual(check_metrics("`zirv_flow_worker_busy`", {"zirv_flow_jobs"}), [
            "unknown metric zirv_flow_worker_busy"
        ])

    def test_sizing_config_keys_are_checked(self):
        self.fixture_repo()
        page = self.root / "docs/operations/sizing.md"
        page.parent.mkdir(parents=True, exist_ok=True)
        page.write_text("`server.public_url`\n")
        self.assertEqual(check(self.root)[0], [])
        page.write_text("`server.public_urll`\n")
        self.assertEqual(check(self.root)[0], [
            "docs/operations/sizing.md: unknown config key or registered type/action server.public_urll"
        ])

    def test_sizing_literal_data_paths_must_exist(self):
        self.fixture_repo()
        page = self.root / "docs/operations/sizing.md"
        page.parent.mkdir(parents=True, exist_ok=True)
        data = self.root / "docs/benchmarks/data/run.json"
        data.parent.mkdir(parents=True)
        data.write_text("{}")
        page.write_text("`docs/benchmarks/data/run.json`\n")
        self.assertEqual(check(self.root)[0], [])
        data.unlink()
        self.assertEqual(check(self.root)[0], [
            "docs/operations/sizing.md: missing benchmark data path docs/benchmarks/data/run.json"
        ])

    def test_sizing_cannot_cite_smoke_or_failed_data(self):
        self.fixture_repo()
        page = self.root / "docs/operations/sizing.md"
        page.parent.mkdir(parents=True, exist_ok=True)
        data = self.root / "docs/benchmarks/data/m8/smoke/x.json"
        data.parent.mkdir(parents=True)
        data.write_text("{}")
        page.write_text("[run](../benchmarks/data/m8/smoke/x.json)\n")
        self.assertEqual(check(self.root)[0], [
            "docs/operations/sizing.md: benchmark data must not cite smoke or failed runs: "
            "docs/benchmarks/data/m8/smoke/x.json"
        ])

    def test_new_guides_have_link_route_and_config_checks(self):
        self.fixture_repo()
        self.assertEqual(len(GUIDES[2:]), 4)
        for guide in GUIDES[2:]:
            with self.subTest(guide=guide):
                page = self.root / guide
                page.write_text("[broken](missing.md)\n`GET /api/v1/missing`\n`server.public_urll`\n")
                errors, _, _ = check(self.root)
                self.assertTrue(any(guide in error and "missing link target" in error for error in errors))
                self.assertIn(f"{guide}: unknown API route /api/v1/missing", errors)
                self.assertIn(f"{guide}: unknown config key or registered type/action server.public_urll", errors)
                page.write_text("# Guide\n## Database sources\n")

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

    def test_repository_urls_in_docs_and_ui_have_local_anchors(self):
        self.page.write_text('# Home\n<a id="sources"></a>\n')
        guide = self.root / "docs/guide/sources.md"
        guide.parent.mkdir(parents=True)
        guide.write_text("# Sources\n## Database sources\n")
        ui = self.root / "web/src/source.tsx"
        ui.parent.mkdir(parents=True)
        ui.write_text('const URL = "https://github.com/Glubiz/zirv-flow#sources";\n')
        self.assertEqual(check_links(self.root, [ui]), [])
        target = "https://github.com/Glubiz/zirv-flow/blob/main/docs/guide/sources.md#database-sources"
        ui.write_text(f'const URL = "{target}";\n')
        self.assertEqual(check_links(self.root, [ui]), [])
        guide.write_text("# Sources\n")
        self.assertEqual(check_links(self.root, [ui]), [
            "web/src/source.tsx:1: missing anchor /docs/guide/sources.md#database-sources"
        ])
        self.page.write_text(f"[guide]({target})\n")
        self.assertEqual(check_links(self.root, [self.page]), [
            "README.md:1: missing anchor /docs/guide/sources.md#database-sources"
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
        self.assertEqual(check(self.root), ([], 8, 2))
        source = self.root / "crates/zirv-flow-core/src/flow/graph.rs"
        source.write_text(source.read_text().replace('    Exit {},', '    Exit {},\n    NewNode {},'))
        self.assertEqual(check(self.root), ([
            "cannot map every NodeKind variant to its serialized name"
        ], 8, 2))

    def test_cli_summary_and_invalid_argument(self):
        self.fixture_repo()
        command = [sys.executable, "-B", str(self.root / "scripts/check-docs.py")]
        result = subprocess.run(command, capture_output=True, text=True, check=False)
        self.assertEqual((result.returncode, result.stdout, result.stderr), (
            0, "Documentation checks passed (8 Markdown pages, 7 guides, 2 node types).\n", ""
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
    print("Documentation checker self-test passed (links, anchors, images, routes, config, metrics, nodes, README budget, benchmarks).")


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
        print(f"Documentation checks passed ({page_count} Markdown pages, {len(GUIDES) + 1} guides, {node_count} node types).")
