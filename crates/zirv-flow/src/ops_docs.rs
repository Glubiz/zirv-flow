//! Checks that tie the operator docs to the code, with no database: the alert rules, the
//! runbooks and the configuration keys they name.
//!
//! `docs/operations/alerts.rules.yml` is kept in a regular shape (one rule per block, `expr` on
//! one line) so a line reader can take it apart without a YAML dependency. A later page adds
//! itself to [`DOC_PAGES`] to have its configuration keys checked.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::config::CONFIG_KEYS;

const PREFIX: &str = "zirv_flow_";
const RULES_FILE: &str = "docs/operations/alerts.rules.yml";
const RUNBOOKS_FILE: &str = "docs/operations/runbooks.md";

/// The doc pages whose backticked configuration keys must exist in `config.rs`.
const DOC_PAGES: [&str; 1] = [RUNBOOKS_FILE];

/// The parts every runbook section has, in order.
const RUNBOOK_PARTS: [&str; 7] = [
    "What it means",
    "User-visible effect",
    "Confirm",
    "Likely causes",
    "Fix",
    "Fixed when",
    "Escalate",
];

/// One alert rule as the line reader sees it.
#[derive(Debug, Default, PartialEq)]
struct Rule {
    alert: String,
    expr: String,
    for_: String,
    severity: String,
    summary: String,
    runbook: String,
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    for quote in ['\'', '"'] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner.to_owned();
        }
    }
    value.to_owned()
}

/// The block of a rule that the reader is inside.
#[derive(Clone, Copy, PartialEq)]
enum Block {
    Rule,
    Labels,
    Annotations,
}

/// Reads the rules of a file in the regular shape, and refuses anything else with the line
/// number. The nesting is fixed by indentation:
///
/// - `groups:` at 0, `- name: X` at 2, `rules:` at 4 and `- alert: X` at 6;
/// - at 8 the keys `expr`, `for`, `labels:` and `annotations:` of a rule;
/// - at 10 `severity` under `labels:`, and `summary` and `runbook` under `annotations:`.
///
/// Each value is on one line, and a key is set at most once.
fn parse_rules(text: &str) -> Result<Vec<Rule>, String> {
    let mut rules: Vec<Rule> = Vec::new();
    let mut groups = false;
    let mut in_group = false;
    let mut has_rules = false;
    let mut block = Block::Rule;
    for (index, line) in text.lines().enumerate() {
        let fail = |what: &str| Err(format!("line {}: {what}: {}", index + 1, line.trim()));
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start_matches(' ').len();
        if line[..indent].contains('\t') || line.trim_start_matches(' ').starts_with('\t') {
            return fail("tabs are not allowed");
        }
        match indent {
            0 if trimmed == "groups:" && !groups => groups = true,
            2 if groups && trimmed.starts_with("- name:") => {
                in_group = true;
                has_rules = false;
            }
            4 if in_group && trimmed == "rules:" && !has_rules => has_rules = true,
            6 if has_rules && trimmed.starts_with("- alert:") => {
                let name = unquote(&trimmed["- alert:".len()..]);
                if name.is_empty() {
                    return fail("the alert has no name");
                }
                rules.push(Rule {
                    alert: name,
                    ..Rule::default()
                });
                block = Block::Rule;
            }
            8 | 10 if !rules.is_empty() && has_rules => {
                let Some((key, value)) = trimmed.split_once(':') else {
                    return fail("not a key");
                };
                let value = value.trim();
                let Some(rule) = rules.last_mut() else {
                    return fail("a key outside a rule");
                };
                let field = match (indent, block, key) {
                    (8, _, "labels" | "annotations") => {
                        if !value.is_empty() {
                            return fail("a block key takes no value");
                        }
                        block = if key == "labels" {
                            Block::Labels
                        } else {
                            Block::Annotations
                        };
                        continue;
                    }
                    (8, _, "expr") => {
                        block = Block::Rule;
                        &mut rule.expr
                    }
                    (8, _, "for") => {
                        block = Block::Rule;
                        &mut rule.for_
                    }
                    (10, Block::Labels, "severity") => &mut rule.severity,
                    (10, Block::Annotations, "summary") => &mut rule.summary,
                    (10, Block::Annotations, "runbook") => &mut rule.runbook,
                    _ => return fail("unknown key or wrong place"),
                };
                if value.is_empty() {
                    return fail("the key has no value");
                }
                if !field.is_empty() {
                    return fail("the key is set twice");
                }
                *field = unquote(value);
            }
            _ => return fail("not in the expected structure"),
        }
    }
    Ok(rules)
}

/// The identifier at the start of `text`.
fn name_at(text: &str) -> String {
    text.chars()
        .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
        .collect()
}

/// The `zirv_flow_` identifiers in `text`.
fn names_in(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for (at, _) in text.match_indices(PREFIX) {
        let name = name_at(&text[at..]);
        if name.len() > PREFIX.len() && !name.ends_with('_') {
            found.insert(name);
        }
    }
    found
}

/// Rust source with its comments and test-only items blanked, and the test-only modules it
/// declares in other files.
struct Production {
    code: String,
    test_mods: Vec<String>,
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Two copies of `source` of the same length in characters: the first with comments blanked, the
/// second also with the insides of string and character literals blanked, so that a search in it
/// sees code only.
fn blank_comments(source: &str) -> (Vec<char>, Vec<char>) {
    let chars: Vec<char> = source.chars().collect();
    let mut clean = chars.clone();
    let mut masked = chars.clone();
    let blank = |text: &mut Vec<char>, from: usize, to: usize| {
        for c in &mut text[from..to] {
            if *c != '\n' {
                *c = ' ';
            }
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            let end = chars[i..]
                .iter()
                .position(|c| *c == '\n')
                .map_or(chars.len(), |p| i + p);
            blank(&mut clean, i, end);
            blank(&mut masked, i, end);
            i = end;
        } else if c == '/' && next == Some('*') {
            let mut depth = 0_u32;
            let mut end = i;
            while end < chars.len() {
                if chars[end] == '/' && chars.get(end + 1) == Some(&'*') {
                    depth += 1;
                    end += 2;
                } else if chars[end] == '*' && chars.get(end + 1) == Some(&'/') {
                    depth -= 1;
                    end += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    end += 1;
                }
            }
            blank(&mut clean, i, end);
            blank(&mut masked, i, end);
            i = end;
        } else if c == 'r' && {
            let before = if i > 0 { chars[i - 1] } else { ' ' };
            let before_b = i > 1 && before == 'b' && !is_ident(chars[i - 2]);
            let hashes = chars[i + 1..].iter().take_while(|c| **c == '#').count();
            (!is_ident(before) || before_b) && chars.get(i + 1 + hashes) == Some(&'"')
        } {
            let hashes = chars[i + 1..].iter().take_while(|c| **c == '#').count();
            let start = i + 2 + hashes;
            let closing: Vec<char> = std::iter::once('"')
                .chain(std::iter::repeat_n('#', hashes))
                .collect();
            let end = (start..chars.len())
                .find(|at| chars[*at..].starts_with(&closing))
                .unwrap_or(chars.len());
            blank(&mut masked, start, end);
            i = (end + closing.len()).min(chars.len());
        } else if c == '"' {
            let mut end = i + 1;
            while end < chars.len() && chars[end] != '"' {
                end += if chars[end] == '\\' { 2 } else { 1 };
            }
            let end = end.min(chars.len());
            blank(&mut masked, i + 1, end);
            i = (end + 1).min(chars.len());
        } else if c == '\'' {
            let literal_end = if next == Some('\\') {
                (i + 3..chars.len().min(i + 12)).find(|at| chars[*at] == '\'')
            } else if chars.get(i + 2) == Some(&'\'') {
                Some(i + 2)
            } else {
                None
            };
            match literal_end {
                Some(end) => {
                    blank(&mut masked, i + 1, end);
                    i = end + 1;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    (clean, masked)
}

/// The index just past the bracket that closes the one at `open`, in `masked`.
fn closing(masked: &[char], open: usize) -> usize {
    let (up, down) = match masked[open] {
        '{' => ('{', '}'),
        '[' => ('[', ']'),
        _ => ('(', ')'),
    };
    let mut depth = 0_u32;
    for (at, c) in masked.iter().enumerate().skip(open) {
        if *c == up {
            depth += 1;
        } else if *c == down {
            depth -= 1;
            if depth == 0 {
                return at + 1;
            }
        }
    }
    masked.len()
}

/// `source` without comments and without items marked `#[cfg(test)]`: an inline module or
/// function by its braces, a declaration such as `mod x;` or a `use` by its semicolon. The
/// names of the `mod x;` declarations are returned, for the caller to skip those files.
fn production_code(source: &str) -> Production {
    let (mut clean, masked) = blank_comments(source);
    let attribute: Vec<char> = "#[cfg(test)]".chars().collect();
    let mut test_mods = Vec::new();
    let mut from = 0;
    while let Some(found) = (from..masked.len()).find(|at| masked[*at..].starts_with(&attribute)) {
        let mut at = found + attribute.len();
        loop {
            while masked.get(at).is_some_and(|c| c.is_whitespace()) {
                at += 1;
            }
            if masked.get(at) == Some(&'#') && masked.get(at + 1) == Some(&'[') {
                at = closing(&masked, at + 1);
            } else {
                break;
            }
        }
        let head: String = masked[at..].iter().take(40).collect();
        let head = head
            .trim_start_matches("pub(crate) ")
            .trim_start_matches("pub ");
        let end = if head.starts_with("use ") {
            (at..masked.len())
                .find(|i| masked[*i] == ';')
                .map_or(masked.len(), |i| i + 1)
        } else {
            let mut depth = 0_u32;
            let mut end = masked.len();
            for i in at..masked.len() {
                match masked[i] {
                    '(' | '[' => depth += 1,
                    ')' | ']' => depth = depth.saturating_sub(1),
                    '{' if depth == 0 => {
                        end = closing(&masked, i);
                        break;
                    }
                    ';' if depth == 0 => {
                        end = i + 1;
                        break;
                    }
                    _ => {}
                }
            }
            end
        };
        if let Some(name) = head.strip_prefix("mod ") {
            let name: String = name.chars().take_while(|c| is_ident(*c)).collect();
            if masked[at..end].last() == Some(&';') {
                test_mods.push(name);
            }
        }
        for c in &mut clean[found..end] {
            if *c != '\n' {
                *c = ' ';
            }
        }
        from = end.max(found + 1);
    }
    Production {
        code: clean.into_iter().collect(),
        test_mods,
    }
}

/// Every series a `counter!`, `gauge!` or `histogram!` call in `source` names, with the label
/// keys of that call.
/// Test-only code and comments do not count.
fn emitted_in(source: &str, into: &mut BTreeMap<String, BTreeSet<String>>) {
    let source = production_code(source).code;
    let source = source.as_str();
    for kind in ["counter", "gauge", "histogram"] {
        let opener = format!("{kind}!(");
        for (at, _) in source.match_indices(&opener) {
            let call = &source[at + opener.len()..];
            let Some(rest) = call.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some(name) = rest.split('"').next().filter(|n| n.starts_with(PREFIX)) else {
                continue;
            };
            let labels = into.entry(name.to_owned()).or_default();
            let mut depth = 1_u32;
            let mut in_string = false;
            let mut end = call.len();
            for (index, c) in call.char_indices() {
                match c {
                    '"' => in_string = !in_string,
                    '(' if !in_string => depth += 1,
                    ')' if !in_string => {
                        depth -= 1;
                        if depth == 0 {
                            end = index;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let body = &call[..end];
            for (arrow, _) in body.match_indices("\" =>") {
                if let Some(start) = body[..arrow].rfind('"') {
                    labels.insert(body[start + 1..arrow].to_owned());
                }
            }
        }
    }
}

/// One series reference in an expression and the label keys its matchers use.
#[derive(Debug, PartialEq)]
struct Selector {
    name: String,
    matchers: Vec<String>,
}

fn selectors(expr: &str) -> Vec<Selector> {
    let mut found = Vec::new();
    for (at, _) in expr.match_indices(PREFIX) {
        let name = name_at(&expr[at..]);
        let after = &expr[at + name.len()..];
        let matchers = match after
            .strip_prefix('{')
            .and_then(|rest| rest.split_once('}'))
        {
            Some((inside, _)) => inside
                .split(',')
                .filter_map(|matcher| {
                    let key: String = matcher
                        .trim()
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    (!key.is_empty()).then_some(key)
                })
                .collect(),
            None => Vec::new(),
        };
        found.push(Selector { name, matchers });
    }
    found
}

/// The label keys named by `by (...)`, `on (...)`, `without (...)` and `ignoring (...)`.
fn grouping_labels(expr: &str) -> Vec<String> {
    let mut found = Vec::new();
    for word in ["by", "on", "without", "ignoring"] {
        for (at, _) in expr.match_indices(word) {
            let before_ok = expr[..at]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
            let rest = expr[at + word.len()..].trim_start();
            if !before_ok || !rest.starts_with('(') {
                continue;
            }
            if let Some((inside, _)) = rest[1..].split_once(')') {
                found.extend(
                    inside
                        .split(',')
                        .map(str::trim)
                        .filter(|label| !label.is_empty())
                        .map(str::to_owned),
                );
            }
        }
    }
    found
}

/// The emitted series a name in a doc refers to: the name itself, or the histogram it is a
/// `_bucket`, `_sum` or `_count` of.
fn resolve<'a>(name: &str, emitted: &'a BTreeMap<String, BTreeSet<String>>) -> Option<&'a str> {
    if let Some((key, _)) = emitted.get_key_value(name) {
        return Some(key);
    }
    ["_bucket", "_sum", "_count"]
        .iter()
        .filter_map(|suffix| name.strip_suffix(suffix))
        .find_map(|base| emitted.get_key_value(base).map(|(key, _)| key.as_str()))
}

/// A heading's anchor on the hosting site: lowercase, with only letters and digits kept (the
/// runbook headings are single words).
fn anchor(heading: &str) -> String {
    heading
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// A `##` section of the runbooks and its `###` parts.
#[derive(Debug, PartialEq)]
struct Section {
    heading: String,
    parts: Vec<String>,
}

fn sections(text: &str) -> Vec<Section> {
    let mut found: Vec<Section> = Vec::new();
    let mut fenced = false;
    for line in text.lines() {
        if line.starts_with("```") {
            fenced = !fenced;
        }
        if fenced {
            continue;
        }
        if let Some(heading) = line.strip_prefix("## ") {
            found.push(Section {
                heading: heading.trim().to_owned(),
                parts: Vec::new(),
            });
        } else if let Some(part) = line.strip_prefix("### ")
            && let Some(section) = found.last_mut()
        {
            section.parts.push(part.trim().to_owned());
        }
    }
    found
}

/// The backticked dotted names in `page` that start with a configuration section but are neither
/// a configuration key nor a job kind.
fn unknown_config_keys(page: &str, keys: &[&str], job_kinds: &BTreeSet<&str>) -> Vec<String> {
    let sections: BTreeSet<&str> = keys
        .iter()
        .filter_map(|key| key.split('.').next())
        .collect();
    let mut unknown = BTreeSet::new();
    for (index, span) in page.split('`').enumerate() {
        let dotted = index % 2 == 1
            && span.contains('.')
            && span.split('.').all(|part| {
                !part.is_empty() && part.chars().all(|c| c.is_ascii_lowercase() || c == '_')
            });
        let first = span.split('.').next().unwrap_or_default();
        if dotted && sections.contains(first) && !keys.contains(&span) && !job_kinds.contains(span)
        {
            unknown.insert(span.to_owned());
        }
    }
    unknown.into_iter().collect()
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative))
        .unwrap_or_else(|error| panic!("cannot read {relative}: {error}"))
}

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("a readable source directory") {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

/// Every series emitted under `crates/`, with the label keys any call site sets.
fn emitted_series() -> BTreeMap<String, BTreeSet<String>> {
    let mut files = Vec::new();
    rust_files(&root().join("crates"), &mut files);
    let sources: Vec<(PathBuf, String)> = files
        .into_iter()
        .filter(|path| !path.components().any(|part| part.as_os_str() == "tests"))
        .map(|path| {
            let source = std::fs::read_to_string(&path).expect("a readable source file");
            (path, source)
        })
        .collect();
    let mut test_only: Vec<PathBuf> = Vec::new();
    for (path, source) in &sources {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let dir = path.parent().unwrap_or(Path::new("."));
        let home = if ["lib", "main", "mod"].contains(&stem) {
            dir.to_path_buf()
        } else {
            dir.join(stem)
        };
        test_only.extend(
            production_code(source)
                .test_mods
                .into_iter()
                .flat_map(|name| [home.join(format!("{name}.rs")), home.join(name)]),
        );
    }
    let mut emitted = BTreeMap::new();
    for (path, source) in &sources {
        if !test_only.iter().any(|skipped| path.starts_with(skipped)) {
            emitted_in(source, &mut emitted);
        }
    }
    emitted
}

fn rules() -> Vec<Rule> {
    parse_rules(&read(RULES_FILE)).expect("the rule file is in the regular shape")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(kind: &str, rest: &str) -> String {
        format!("metrics::{kind}!({rest}")
    }

    #[test]
    fn the_reader_takes_the_fields_of_each_rule() {
        let text = "# header
groups:
  - name: g
    rules:
      - alert: First
        expr: 'sum by (kind) (zirv_flow_jobs{state=\"dead\"}) > 0'
        for: 10m
        labels:
          severity: warning
        annotations:
          summary: 'Jobs: dead'
          runbook: 'docs/operations/runbooks.md#first'
      - alert: Second
        expr: up == 0
        for: 1m
";

        let rules = parse_rules(text).unwrap();

        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].alert, "First");
        assert_eq!(
            rules[0].expr,
            "sum by (kind) (zirv_flow_jobs{state=\"dead\"}) > 0"
        );
        assert_eq!(rules[0].for_, "10m");
        assert_eq!(rules[0].severity, "warning");
        assert_eq!(rules[0].summary, "Jobs: dead");
        assert_eq!(rules[0].runbook, "docs/operations/runbooks.md#first");
        assert_eq!(rules[1].expr, "up == 0");
        assert_eq!(rules[1].runbook, "");
    }

    const SHAPE: &str = "groups:\n  - name: g\n    rules:\n      - alert: A\n";

    fn with(extra: &str) -> String {
        format!("{SHAPE}{extra}")
    }

    #[test]
    fn the_reader_refuses_a_key_set_twice() {
        let text = with("        for: 1m\n        for: 2m\n");

        let error = parse_rules(&text).unwrap_err();

        assert!(
            error.starts_with("line 6:") && error.contains("set twice"),
            "{error}"
        );
    }

    #[test]
    fn the_reader_refuses_a_misspelled_level_with_its_line_number() {
        let text = "groups:\n  - name: g\n    ruless:\n";

        let error = parse_rules(text).unwrap_err();

        assert!(error.starts_with("line 3:"), "{error}");
    }

    #[test]
    fn the_reader_refuses_an_unknown_key_in_a_rule() {
        let text = with("        expr: up\n        fro: 5m\n");

        let error = parse_rules(&text).unwrap_err();

        assert!(
            error.starts_with("line 6:") && error.contains("unknown key"),
            "{error}"
        );
    }

    #[test]
    fn the_reader_refuses_a_child_under_the_wrong_block() {
        let text = with("        labels:\n          summary: x\n");

        assert!(parse_rules(&text).unwrap_err().starts_with("line 6:"));
    }

    #[test]
    fn the_reader_refuses_rules_outside_a_group_and_misplaced_indentation() {
        assert!(parse_rules("rules:\n").unwrap_err().starts_with("line 1:"));
        assert!(
            parse_rules("groups:\n  - name: g\n    rules:\n     - alert: A\n")
                .unwrap_err()
                .starts_with("line 4:")
        );
        assert!(
            parse_rules("groups:\n  - alert: A\n")
                .unwrap_err()
                .starts_with("line 2:")
        );
    }

    #[test]
    fn a_comment_or_a_test_only_call_is_not_an_emitted_series() {
        let source = format!(
            "// {}\"zirv_flow_in_comment\");\n/* {}\"zirv_flow_in_block\"); */\nlet s = \"// not a comment {}\\\"zirv_flow_in_string\\\")\";\n\
             {}\"zirv_flow_real\");\n\
             #[cfg(test)]\nmod tests {{\n    fn t() {{ {}\"zirv_flow_in_tests\"); }}\n}}\n\
             #[cfg(test)]\nfn only_in_tests() {{ {}\"zirv_flow_in_test_fn\"); }}\n\
             #[cfg(test)]\nmod elsewhere;\n\
             {}\"zirv_flow_after\", \"label\" => 1);\n",
            call("gauge", ""),
            call("gauge", ""),
            call("gauge", ""),
            call("gauge", ""),
            call("gauge", ""),
            call("gauge", ""),
            call("counter", ""),
        );
        let mut emitted = BTreeMap::new();

        emitted_in(&source, &mut emitted);

        assert_eq!(
            emitted.keys().map(String::as_str).collect::<Vec<_>>(),
            ["zirv_flow_after", "zirv_flow_real"]
        );
        assert!(emitted["zirv_flow_after"].contains("label"));
    }

    #[test]
    fn test_only_module_declarations_are_named_and_end_at_the_semicolon() {
        let source = "#[cfg(test)]\nmod e2e;\n#[cfg(test)]\nuse a::{b, c};\nmod live { fn f() {} }\nfn kept() {}\n";

        let production = production_code(source);

        assert_eq!(production.test_mods, ["e2e"]);
        assert!(production.code.contains("mod live") && production.code.contains("fn kept"));
        assert!(!production.code.contains("e2e") && !production.code.contains("use a"));
    }

    #[test]
    fn selectors_and_groupings_are_read_from_an_expression() {
        let expr = "sum by (le, kind) (rate(zirv_flow_job_claim_lag_seconds_bucket{kind=~\"a|b\", x!=\"y\"}[5m])) \
                    / on (kind) group_left zirv_flow_jobs and bypass(1)";

        assert_eq!(
            selectors(expr),
            [
                Selector {
                    name: "zirv_flow_job_claim_lag_seconds_bucket".into(),
                    matchers: vec!["kind".into(), "x".into()],
                },
                Selector {
                    name: "zirv_flow_jobs".into(),
                    matchers: vec![],
                },
            ]
        );
        assert_eq!(grouping_labels(expr), ["le", "kind", "kind"]);
    }

    #[test]
    fn emitted_series_carry_the_labels_of_their_call() {
        let source = format!(
            "{}\n    \"zirv_flow_a\",\n    \"pool\" => name.to_owned(),\n    \"state\" => \"idle\",\n)\n{}\"zirv_flow_b\").set(1.0);\n{}\"not_ours\", \"x\" => 1);",
            call("gauge", ""),
            call("counter", ""),
            call("gauge", ""),
        );
        let mut emitted = BTreeMap::new();

        emitted_in(&source, &mut emitted);

        assert_eq!(
            emitted.keys().map(String::as_str).collect::<Vec<_>>(),
            ["zirv_flow_a", "zirv_flow_b"]
        );
        assert_eq!(
            emitted["zirv_flow_a"]
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["pool", "state"]
        );
        assert!(emitted["zirv_flow_b"].is_empty());
    }

    #[test]
    fn histogram_suffixes_resolve_to_the_emitted_series() {
        let mut emitted = BTreeMap::new();
        emitted.insert("zirv_flow_h".to_owned(), BTreeSet::new());

        assert_eq!(resolve("zirv_flow_h_bucket", &emitted), Some("zirv_flow_h"));
        assert_eq!(resolve("zirv_flow_h", &emitted), Some("zirv_flow_h"));
        assert_eq!(resolve("zirv_flow_other", &emitted), None);
    }

    #[test]
    fn anchors_are_lowercase_alphanumerics() {
        assert_eq!(anchor("ZirvFlowJobsBacklog"), "zirvflowjobsbacklog");
    }

    #[test]
    fn sections_list_their_parts_and_skip_fenced_headings() {
        let text = "# Title\n## One\n### A\n```\n## not a section\n```\n### B\n## Two\n";

        assert_eq!(
            sections(text),
            [
                Section {
                    heading: "One".into(),
                    parts: vec!["A".into(), "B".into()],
                },
                Section {
                    heading: "Two".into(),
                    parts: vec![],
                },
            ]
        );
    }

    #[test]
    fn unknown_config_keys_skip_keys_job_kinds_and_other_names() {
        let keys = ["worker.concurrency", "ingest.drain_shards"];
        let kinds = BTreeSet::from(["ingest.drain"]);
        let page = "`worker.concurrency` `worker.concurency` `ingest.drain` `ingest.nope` \
                    `flow.step` `docs/x.md` `a.b.c`";

        assert_eq!(
            unknown_config_keys(page, &keys, &kinds),
            ["ingest.nope", "worker.concurency"]
        );
    }

    #[test]
    fn every_series_in_the_rules_is_emitted_by_the_code() {
        let emitted = emitted_series();
        assert!(
            emitted.len() > 50,
            "the scan found only {:?}",
            emitted.keys()
        );
        let rules = rules();
        assert!(rules.len() >= 10, "only {} rules", rules.len());

        let unknown: Vec<String> = rules
            .iter()
            .flat_map(|rule| names_in(&rule.expr))
            .filter(|name| resolve(name, &emitted).is_none())
            .collect();

        assert!(
            unknown.is_empty(),
            "series the code does not emit: {unknown:?}"
        );
    }

    #[test]
    fn every_series_the_runbooks_name_is_emitted_by_the_code() {
        let emitted = emitted_series();

        let unknown: Vec<String> = names_in(&read(RUNBOOKS_FILE))
            .into_iter()
            .filter(|name| resolve(name, &emitted).is_none())
            .collect();

        assert!(
            unknown.is_empty(),
            "series the code does not emit: {unknown:?}"
        );
    }

    #[test]
    fn no_rule_uses_a_label_the_code_does_not_set() {
        let emitted = emitted_series();
        let mut bad = Vec::new();
        for rule in rules() {
            let mut known: BTreeSet<&str> = BTreeSet::from(["le"]);
            for selector in selectors(&rule.expr) {
                let Some(series) = resolve(&selector.name, &emitted) else {
                    continue;
                };
                known.extend(emitted[series].iter().map(String::as_str));
                for label in &selector.matchers {
                    if !emitted[series].contains(label) {
                        bad.push(format!("{}: {} has no label {label}", rule.alert, series));
                    }
                }
            }
            for label in grouping_labels(&rule.expr) {
                if !known.contains(label.as_str()) {
                    bad.push(format!("{}: no series sets the label {label}", rule.alert));
                }
            }
        }

        assert!(bad.is_empty(), "labels the code does not set: {bad:#?}");
    }

    #[test]
    fn every_alert_is_complete() {
        for rule in rules() {
            assert!(!rule.alert.is_empty() && !rule.expr.is_empty(), "{rule:?}");
            assert!(
                rule.for_.ends_with('m') || rule.for_.ends_with('h'),
                "{rule:?}"
            );
            assert!(
                ["warning", "critical"].contains(&rule.severity.as_str()),
                "{rule:?}"
            );
            assert!(!rule.summary.is_empty(), "{rule:?}");
        }
    }

    #[test]
    fn every_alert_links_to_a_runbook_section_and_every_section_has_an_alert() {
        let rules = rules();
        let runbooks = read(RUNBOOKS_FILE);
        let sections = sections(&runbooks);

        let linked: Vec<String> = rules
            .iter()
            .map(|rule| {
                rule.runbook
                    .strip_prefix(&format!("{RUNBOOKS_FILE}#"))
                    .unwrap_or_else(|| panic!("{} has no runbook link", rule.alert))
                    .to_owned()
            })
            .collect();
        let anchors: Vec<String> = sections.iter().map(|s| anchor(&s.heading)).collect();

        assert_eq!(linked, anchors, "alerts and runbook sections, in order");
        let names: Vec<&str> = rules.iter().map(|rule| rule.alert.as_str()).collect();
        let headings: Vec<&str> = sections.iter().map(|s| s.heading.as_str()).collect();
        assert_eq!(names, headings);
    }

    #[test]
    fn every_runbook_section_has_the_fixed_layout() {
        for section in sections(&read(RUNBOOKS_FILE)) {
            assert_eq!(section.parts, RUNBOOK_PARTS, "{}", section.heading);
        }
    }

    #[test]
    fn the_doc_pages_name_only_configuration_keys_that_exist() {
        let registry = zirv_flow_jobs::Registry::built_in();
        let kinds = registry.kinds();
        let job_kinds: BTreeSet<&str> = kinds.iter().copied().collect();
        assert!(job_kinds.contains("ingest.drain"));

        for page in DOC_PAGES {
            let unknown = unknown_config_keys(&read(page), &CONFIG_KEYS, &job_kinds);

            assert!(unknown.is_empty(), "{page} names unknown keys: {unknown:?}");
        }
    }
}
