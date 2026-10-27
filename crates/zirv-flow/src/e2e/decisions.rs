//! The decision registry's guards (#382): a scan of the production sources that no decision is
//! written outside the registry, and the table that maps every registry row to the test that
//! produces it. The registry is `zirv_flow_core::decision_actions`.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use hashbrown::HashSet;
    use serde_json::{Value, json};
    use zirv_flow_core::audit;
    use zirv_flow_core::decision_actions::{self, DecisionSpec};

    /// The only production file that builds a `DecisionSpec`.
    const SPEC_BUILDERS: [&str; 1] = ["zirv-flow-core/src/decision_actions.rs"];

    /// The only production files that insert into the decision log with SQL of their own; each
    /// binds its action and outcome from the registry.
    const SQL_WRITERS: [&str; 3] = [
        "zirv-flow-store/src/decisions.rs",
        "zirv-flow-store/src/jobs.rs",
        "zirv-flow-store/src/segments/refresh.rs",
    ];

    /// What produces a registry row in a test.
    #[derive(Clone, Copy, Debug)]
    enum Coverage {
        /// The name of a test that makes the decision and checks it.
        Test(&'static str),
        /// No test can make it, and why.
        Untestable(&'static str),
        /// A fixture that issue #384 supplies.
        Pending(u32),
    }

    use Coverage::{Pending, Test};

    const SEND_PATH: &str = "the_send_path_and_its_trace_hold_end_to_end";

    /// Every registry row, by constant name, with what covers it.
    #[rustfmt::skip]
    const COVERAGE: &[(&str, Coverage)] = &[
        ("EVENT_ACCEPTED_VALID", Test(SEND_PATH)),
        ("EVENT_ACCEPTED_FLAGGED", Pending(384)),
        ("EVENT_ACCEPTED_UNPLANNED", Pending(384)),
        ("EVENT_REJECTED", Pending(384)),
        ("RECORD_REJECTED_APPLY_FAILED", Pending(384)),
        ("EVENT_REPLAYED_VALID", Pending(384)),
        ("EVENT_REPLAYED_FLAGGED", Pending(384)),
        ("EVENT_REPLAYED_UNPLANNED", Pending(384)),
        ("PERSON_CREATED", Pending(384)),
        ("TRAIT_CHANGED", Pending(384)),
        ("TRAIT_CHANGED_DEFINITION", Pending(384)),
        ("TRAIT_CHANGED_MERGED", Pending(384)),
        ("CONTRIBUTION_REMOVED", Pending(384)),
        ("IDENTITY_SPLIT", Pending(384)),
        ("IDENTITY_LIMIT", Pending(384)),
        ("IDENTITY_CONFLICT_RECORDED", Pending(384)),
        ("IDENTITY_CONFLICT_RESOLVED_KEPT_SEPARATE", Pending(384)),
        ("IDENTITY_CONFLICT_RESOLVED_IDENTIFIER_MOVED", Pending(384)),
        ("PERSON_MERGED", Pending(384)),
        ("PERSON_MERGE_REWRITE_COMPLETED", Pending(384)),
        ("PERSON_DELETED", Pending(384)),
        ("CONSENT_CHANGED", Test(SEND_PATH)),
        ("CONSENT_SUPERSEDED", Pending(384)),
        ("CONSENT_POLICY_PUBLISHED", Pending(384)),
        ("LIST_MEMBERSHIP_ADDED", Pending(384)),
        ("LIST_MEMBERSHIP_REMOVED", Pending(384)),
        ("LIST_MEMBERSHIP_MOVED", Pending(384)),
        ("TEMPLATE_PUBLISHED", Pending(384)),
        ("SENDING_DOMAIN_VERIFIED", Pending(384)),
        ("SENDING_DOMAIN_UNVERIFIED", Pending(384)),
        ("FLOW_PUBLISHED", Pending(384)),
        ("FLOW_PAUSED", Pending(384)),
        ("FLOW_RESUMED", Pending(384)),
        ("FLOW_ARCHIVED", Pending(384)),
        ("FLOW_ENROLLED", Test(SEND_PATH)),
        ("FLOW_ENROLL_SKIPPED", Pending(384)),
        ("FLOW_ENROLLMENT_COMPLETED", Test(SEND_PATH)),
        ("FLOW_ENROLLMENT_EXITED", Pending(384)),
        ("FLOW_ENROLLMENT_FAILED", Pending(384)),
        ("FLOW_ENROLLMENT_HELD", Pending(384)),
        ("FLOW_ENROLLMENT_RESUMED", Pending(384)),
        ("FLOW_ENROLLMENT_RETRIED", Pending(384)),
        ("FLOW_ENROLLMENT_SKIPPED", Pending(384)),
        ("FLOW_STEP_DONE", Test(SEND_PATH)),
        ("FLOW_STEP_ENQUEUED", Test(SEND_PATH)),
        ("FLOW_STEP_WAITING", Test(SEND_PATH)),
        ("FLOW_STEP_FAILED", Pending(384)),
        ("FLOW_WAKE_EVENT", Pending(384)),
        ("FLOW_WAKE_TIMEOUT", Pending(384)),
        ("FLOW_WAKE_TIMER", Test(SEND_PATH)),
        ("FLOW_WAIT_UNTIL_EVENT", Pending(384)),
        ("FLOW_WAIT_UNTIL_TIME", Pending(384)),
        ("FLOW_WAIT_DURATION", Pending(384)),
        ("FLOW_GOAL_CONVERTED", Pending(384)),
        ("FLOW_BRANCH_CASE_0", Pending(384)),
        ("FLOW_BRANCH_CASE_1", Pending(384)),
        ("FLOW_BRANCH_CASE_2", Pending(384)),
        ("FLOW_BRANCH_CASE_3", Pending(384)),
        ("FLOW_BRANCH_CASE_4", Pending(384)),
        ("FLOW_BRANCH_CASE_5", Pending(384)),
        ("FLOW_BRANCH_CASE_6", Pending(384)),
        ("FLOW_BRANCH_CASE_7", Pending(384)),
        ("FLOW_BRANCH_CASE_8", Pending(384)),
        ("FLOW_BRANCH_CASE_9", Pending(384)),
        ("FLOW_BRANCH_CASE_MORE", Pending(384)),
        ("FLOW_BRANCH_ELSE", Pending(384)),
        ("FLOW_BRANCH_CONDITION_MATCHED", Pending(384)),
        ("FLOW_BRANCH_CONDITION_UNMATCHED", Pending(384)),
        ("SEND_GATE_PASSED", Test(SEND_PATH)),
        ("SEND_GATE_BLOCKED", Test(SEND_PATH)),
        ("MESSAGE_SEND_SENT", Test(SEND_PATH)),
        ("MESSAGE_SEND_SKIPPED", Test(SEND_PATH)),
        ("MESSAGE_SEND_FAILED", Pending(384)),
        ("MESSAGE_SEND_DROPPED", Pending(384)),
        ("MESSAGE_SEND_DEFERRED", Pending(384)),
        ("MESSAGE_SEND_NOTED", Pending(384)),
        ("MESSAGE_FEEDBACK_DELIVERED", Test(SEND_PATH)),
        ("MESSAGE_FEEDBACK_BOUNCED_HARD", Pending(384)),
        ("MESSAGE_FEEDBACK_BOUNCED_SOFT", Pending(384)),
        ("MESSAGE_FEEDBACK_COMPLAINED", Pending(384)),
        ("SUPPRESSION_FEEDBACK_ORGANIZATION", Pending(384)),
        ("SUPPRESSION_FEEDBACK_WORKSPACE", Pending(384)),
        ("SUPPRESSION_FEEDBACK_UNABLE", Pending(384)),
        ("WEBHOOK_CALL_SUCCEEDED", Pending(384)),
        ("WEBHOOK_CALL_FAILED", Pending(384)),
        ("SYNC_UPSERT_SUCCEEDED", Pending(384)),
        ("SYNC_UPSERT_FAILED", Pending(384)),
        ("SYNC_REMOVE_SUCCEEDED", Pending(384)),
        ("SYNC_REMOVE_FAILED", Pending(384)),
        ("EVENT_FORWARD_RETRY", Pending(384)),
        ("EVENT_FORWARD_DEAD", Pending(384)),
        ("SYNC_RECORD_APPLIED", Pending(384)),
        ("SYNC_RECORD_DELETED", Pending(384)),
        ("SYNC_RECORD_REJECTED", Pending(384)),
        ("SEGMENT_ENTERED", Test(SEND_PATH)),
        ("SEGMENT_EXITED", Pending(384)),
        ("SEGMENT_REFRESH_SKIPPED", Pending(384)),
        ("SEGMENT_REFRESH_FAILED", Pending(384)),
        ("SEGMENT_REEVALUATE_FAILED", Pending(384)),
        ("JOB_ATTEMPT_FAILED", Pending(384)),
        ("JOB_ATTEMPT_EXHAUSTED", Pending(384)),
        ("JOB_ATTEMPT_LEASE_EXPIRED", Pending(384)),
        ("JOB_RETRY_REQUEUED", Pending(384)),
    ];

    fn crates_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
    }

    fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("the source directory reads")
            .map(|entry| entry.expect("a directory entry reads").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                rust_files(&path, found);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }

    /// The index just past the item whose `{` is the first one at or after `from`: the matching
    /// `}`. Comments, strings, raw strings and character literals do not count their braces.
    fn after_block(text: &str, from: usize) -> usize {
        let bytes = text.as_bytes();
        let mut depth = 0usize;
        let mut at = from;
        while at < bytes.len() {
            match bytes[at] {
                b'/' if bytes.get(at + 1) == Some(&b'/') => {
                    at += text[at..].find('\n').unwrap_or(bytes.len() - at);
                }
                b'/' if bytes.get(at + 1) == Some(&b'*') => {
                    at += text[at + 2..]
                        .find("*/")
                        .map_or(bytes.len() - at, |end| end + 4);
                }
                b'"' => {
                    at += 1;
                    while at < bytes.len() && bytes[at] != b'"' {
                        at += if bytes[at] == b'\\' { 2 } else { 1 };
                    }
                    at += 1;
                }
                b'r' if matches!(bytes.get(at + 1), Some(b'"' | b'#'))
                    && !(at > 0
                        && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_')) =>
                {
                    let hashes = bytes[at + 1..].iter().take_while(|b| **b == b'#').count();
                    if bytes.get(at + 1 + hashes) == Some(&b'"') {
                        let close = format!("\"{}", "#".repeat(hashes));
                        at += 2 + hashes;
                        at += text[at..]
                            .find(&close)
                            .map_or(bytes.len() - at, |end| end + close.len());
                    } else {
                        at += 1;
                    }
                }
                b'\'' => {
                    // A character literal, unless it is a lifetime.
                    if bytes.get(at + 1) == Some(&b'\\') {
                        at += 2 + text[at + 2..].find('\'').map_or(0, |end| end + 1);
                    } else if bytes.get(at + 2) == Some(&b'\'') {
                        at += 3;
                    } else {
                        at += 1;
                    }
                }
                b'{' => {
                    depth += 1;
                    at += 1;
                }
                b'}' => {
                    depth = depth.saturating_sub(1);
                    at += 1;
                    if depth == 0 {
                        return at;
                    }
                }
                _ => at += 1,
            }
        }
        bytes.len()
    }

    /// The module line that follows the `#[cfg(test)]` line at `lines[at]`, skipping any further
    /// attribute lines: its index and whether it declares a file module (`mod name;`).
    fn test_module_after(lines: &[&str], at: usize) -> Option<(usize, bool)> {
        let mut next = at + 1;
        while lines
            .get(next)
            .is_some_and(|line| line.trim_start().starts_with("#["))
        {
            next += 1;
        }
        let line = lines.get(next)?.trim_start();
        let line = line
            .strip_prefix("pub")
            .map_or(line, |rest| match rest.strip_prefix('(') {
                Some(restricted) => restricted.split_once(')').map_or(rest, |(_, tail)| tail),
                None => rest,
            })
            .trim_start();
        let declared = line.strip_prefix("mod ")?;
        Some((next, declared.trim_end().ends_with(';')))
    }

    /// The text of a file without the bodies of its inline `#[cfg(test)] mod name { .. }`.
    /// A `#[cfg(test)] mod name;` declaration has no body here and is left alone.
    fn production(text: &str) -> String {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let mut starts = Vec::with_capacity(lines.len());
        let mut offset = 0;
        for line in &lines {
            starts.push(offset);
            offset += line.len();
        }
        let mut kept = String::new();
        let mut at = 0;
        while at < lines.len() {
            if lines[at].trim_end() == "#[cfg(test)]"
                && let Some((module, false)) = test_module_after(&lines, at)
            {
                let end = after_block(text, starts[module]);
                at = starts.partition_point(|start| *start < end);
                continue;
            }
            kept.push_str(lines[at]);
            at += 1;
        }
        kept
    }

    /// The names of the file modules a file declares behind `#[cfg(test)]`.
    fn test_module_files(text: &str) -> Vec<String> {
        let lines: Vec<&str> = text.lines().collect();
        (0..lines.len())
            .filter(|at| lines[*at].trim_end() == "#[cfg(test)]")
            .filter_map(|at| match test_module_after(&lines, at)? {
                (module, true) => {
                    let line = lines[module].trim_end().trim_end_matches(';');
                    Some(line.rsplit(' ').next()?.to_owned())
                }
                _ => None,
            })
            .collect()
    }

    /// Whether a whole file is test support: the acceptance and test-helper modules, which their
    /// parents declare behind `#[cfg(test)]` or whose content is gated.
    fn is_test_file(path: &Path) -> bool {
        path.file_name()
            .is_some_and(|name| name == "acceptance.rs" || name == "testing.rs")
    }

    /// The production source of every crate: (path relative to `crates/`, text). A file a parent
    /// declares as `#[cfg(test)] mod name;` is test code and is left out.
    fn production_sources() -> Vec<(String, String)> {
        let mut found = Vec::new();
        let mut crates: Vec<_> = std::fs::read_dir(crates_dir())
            .expect("the crates directory reads")
            .map(|entry| entry.expect("a directory entry reads").path())
            .filter(|path| path.join("src").is_dir())
            .collect();
        crates.sort();
        for krate in crates {
            rust_files(&krate.join("src"), &mut found);
        }
        let texts: Vec<(PathBuf, String)> = found
            .into_iter()
            .map(|path| {
                let text = std::fs::read_to_string(&path).expect("a source file reads");
                (path, text)
            })
            .collect();
        let mut test_files: HashSet<PathBuf> = HashSet::new();
        for (path, text) in &texts {
            let stem = path.file_stem().and_then(|stem| stem.to_str());
            let dir = path.parent().expect("a file has a directory");
            let module_dir = match stem {
                Some("mod" | "lib" | "main") | None => dir.to_path_buf(),
                Some(stem) => dir.join(stem),
            };
            for name in test_module_files(text) {
                test_files.insert(module_dir.join(format!("{name}.rs")));
                test_files.insert(module_dir.join(&name).join("mod.rs"));
            }
        }
        texts
            .into_iter()
            .filter(|(path, _)| !is_test_file(path) && !test_files.contains(path))
            .map(|(path, text)| {
                let relative = path
                    .strip_prefix(crates_dir())
                    .expect("the file is under crates")
                    .to_string_lossy()
                    .replace('\\', "/");
                (relative, production(&text))
            })
            .collect()
    }

    fn is_registered(action: &str) -> bool {
        decision_actions::ALL
            .iter()
            .any(|spec| spec.action() == action)
    }

    /// The `action: "..."` literals of `text` that name neither a decision nor an audit action.
    fn unregistered_actions(text: &str) -> Vec<String> {
        let mut found = Vec::new();
        let mut rest = text;
        while let Some(at) = rest.find("action: \"") {
            let after = &rest[at + "action: \"".len()..];
            let Some(end) = after.find('"') else { break };
            let action = &after[..end];
            if !is_registered(action) && !audit::is_known(action) {
                found.push(action.to_owned());
            }
            rest = &after[end..];
        }
        found
    }

    /// Whether `text` writes a `DecisionSpec { .. }` literal, as opposed to naming the type in
    /// a signature.
    fn builds_a_spec(text: &str) -> bool {
        text.match_indices("DecisionSpec {").any(|(at, _)| {
            let before = text[..at].trim_end_matches("decision_actions::").trim_end();
            !(before.ends_with("->") || before.ends_with("'static") || before.ends_with('&'))
        })
    }

    /// Whether `text` holds an `INSERT INTO decision_records`, whatever its case, spacing or
    /// schema prefix.
    fn inserts_into_the_log(text: &str) -> bool {
        let words: Vec<String> = text
            .split_whitespace()
            .map(|word| word.to_ascii_lowercase())
            .collect();
        words.windows(3).any(|at| {
            at[0].ends_with("insert")
                && at[1] == "into"
                && at[2]
                    .trim_start_matches("public.")
                    .trim_start_matches('"')
                    .starts_with("decision_records")
        })
    }

    /// What is wrong with one production file, as messages.
    fn problems(path: &str, text: &str) -> Vec<String> {
        let mut problems: Vec<String> = unregistered_actions(text)
            .into_iter()
            .map(|action| format!("{path}: the action \"{action}\" is not in the registry"))
            .collect();
        if inserts_into_the_log(text) && !SQL_WRITERS.contains(&path) {
            problems.push(format!(
                "{path}: writes decision_records outside the registry"
            ));
        }
        if builds_a_spec(text) && !SPEC_BUILDERS.contains(&path) {
            problems.push(format!("{path}: builds a DecisionSpec of its own"));
        }
        problems
    }

    /// The names of the rows no production code uses.
    fn unused_rows(sources: &[(String, String)]) -> Vec<&'static str> {
        let uses = |name: &str| {
            sources.iter().any(|(_, text)| {
                text.match_indices(name).any(|(at, _)| {
                    let before = text[..at].chars().next_back();
                    let after = text[at + name.len()..].chars().next();
                    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
                    !word(before) && !word(after)
                })
            })
        };
        let branch_cases = uses("BRANCH_CASES");
        decision_actions::NAMES
            .iter()
            .copied()
            .filter(|name| {
                let by_table = branch_cases
                    && name.starts_with("FLOW_BRANCH_CASE_")
                    && *name != "FLOW_BRANCH_CASE_MORE";
                !by_table && !uses(name)
            })
            .collect()
    }

    /// The names of the functions of `text` that a test attribute precedes.
    fn test_names_in(text: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut tested = false;
        for line in text.lines() {
            let line = line.trim_start();
            if line.starts_with("#[") {
                tested |= ["#[test]", "#[tokio::test", "#[sqlx::test"]
                    .iter()
                    .any(|attribute| line.starts_with(attribute));
            } else if let Some(at) = line.find("fn ").filter(|_| tested) {
                names.push(
                    line[at + 3..]
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect(),
                );
                tested = false;
            } else if !(line.is_empty() || line.starts_with("//")) {
                tested = false;
            }
        }
        names
    }

    /// The names of every test function the workspace defines.
    fn test_function_names() -> HashSet<String> {
        let mut found = Vec::new();
        rust_files(&crates_dir(), &mut found);
        let mut names = HashSet::new();
        for path in found {
            if path.components().any(|part| part.as_os_str() == "target") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("a source file reads");
            names.extend(test_names_in(&text));
        }
        names
    }

    /// What is wrong with a coverage table, as messages: a row with no entry, an entry for no
    /// row, a repeated entry, and a test that is not a test function.
    fn coverage_problems(
        table: &[(&str, Coverage)],
        names: &[&str],
        tests: &HashSet<String>,
    ) -> Vec<String> {
        let mut problems = Vec::new();
        for name in names {
            let entries = table.iter().filter(|(row, _)| row == name).count();
            if entries == 0 {
                problems.push(format!(
                    "{name} has no test, no Untestable and no Pending entry"
                ));
            } else if entries > 1 {
                problems.push(format!("{name} has {entries} entries"));
            }
        }
        for (row, coverage) in table {
            if !names.contains(row) {
                problems.push(format!("{row} is not a registry row"));
            }
            match coverage {
                Coverage::Test(test) if !tests.contains(*test) => {
                    problems.push(format!("{row}: {test} is not a test function"));
                }
                Coverage::Untestable("") => {
                    problems.push(format!("{row}: Untestable needs a reason"));
                }
                _ => {}
            }
        }
        problems
    }

    #[test]
    fn no_production_source_writes_a_decision_outside_the_registry() {
        let sources = production_sources();
        assert!(
            sources.len() > 100,
            "the scan found {} files",
            sources.len()
        );

        let problems: Vec<String> = sources
            .iter()
            .flat_map(|(path, text)| problems(path, text))
            .collect();

        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn every_registry_row_is_used_by_production_code() {
        let unused = unused_rows(&production_sources());

        assert!(unused.is_empty(), "rows no code constructs: {unused:?}");
    }

    #[test]
    fn the_scan_fails_on_an_action_string_outside_the_registry() {
        let scratch = "NewDecision { action: \"flow.step\", outcome: \"x\" } \
                       NewAuditEntry { action: \"flow.created\" } \
                       NewDecision { action: \"made.up\" }";

        assert_eq!(unregistered_actions(scratch), ["made.up"]);
        assert_eq!(
            problems(
                "zirv-flow-api/src/x.rs",
                "INSERT INTO decision_records (id)"
            ),
            ["zirv-flow-api/src/x.rs: writes decision_records outside the registry"]
        );
        assert_eq!(
            problems(
                "zirv-flow-api/src/x.rs",
                "let spec = DecisionSpec { action }"
            ),
            ["zirv-flow-api/src/x.rs: builds a DecisionSpec of its own"]
        );
        assert!(
            problems(
                "zirv-flow-store/src/jobs.rs",
                "INSERT INTO decision_records"
            )
            .is_empty()
        );
    }

    #[test]
    fn the_scan_ignores_the_inline_tests_of_a_file() {
        let text = "fn a() {}\n#[cfg(test)]\nmod tests {\n    action: \"made.up\"\n}\n";

        assert_eq!(production(text), "fn a() {}\n");
        assert_eq!(production("fn a() {}\n"), "fn a() {}\n");
    }

    #[test]
    fn the_scan_still_reads_a_writer_after_an_inline_test_module() {
        let text = "#[cfg(test)]\nmod tests {\n    fn f() { let s = \"}\"; let c = '}'; }\n}\n\
                    fn later() { sqlx::query(\"INSERT INTO decision_records (id)\"); }\n";

        let found = problems("zirv-flow-api/src/x.rs", &production(text));

        assert_eq!(
            found,
            ["zirv-flow-api/src/x.rs: writes decision_records outside the registry"]
        );
    }

    #[test]
    fn a_test_module_declaration_does_not_end_the_scan() {
        let text = "#[cfg(test)]\nmod acceptance;\nfn later() { let _ = 1; }\n";

        assert!(production(text).contains("fn later"));
        assert_eq!(test_module_files(text), ["acceptance"]);
        assert!(test_module_files("#[cfg(test)]\nmod t {\n}\n").is_empty());
    }

    #[test]
    fn the_scan_catches_a_lower_case_multi_line_insert() {
        let text = "sqlx::query(\"insert into\n    decision_records (id)\");";

        assert_eq!(
            problems("zirv-flow-api/src/x.rs", text),
            ["zirv-flow-api/src/x.rs: writes decision_records outside the registry"]
        );
        assert!(!inserts_into_the_log("INSERT INTO audit_entries (id)"));
    }

    #[test]
    fn a_row_no_code_uses_is_found() {
        let sources = vec![(
            "x.rs".to_owned(),
            "use a::FLOW_STEP_DONE; let _ = EVENT_ACCEPTED_VALID_X;".to_owned(),
        )];

        let unused = unused_rows(&sources);

        assert!(!unused.contains(&"FLOW_STEP_DONE"));
        assert!(
            unused.contains(&"EVENT_ACCEPTED_VALID"),
            "a longer name is not a use"
        );
    }

    #[test]
    fn every_registry_row_has_a_producing_test_or_a_listed_gap() {
        let problems = coverage_problems(COVERAGE, decision_actions::NAMES, &test_function_names());

        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn the_coverage_table_fails_on_a_row_with_no_entry_and_on_a_missing_test() {
        let names = ["A_ROW", "B_ROW"];
        let tests: HashSet<String> = ["real_test".to_owned()].into_iter().collect();

        let table: &[(&str, Coverage)] = &[("A_ROW", Test("no_such_test"))];
        let problems = coverage_problems(table, &names, &tests);

        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("B_ROW has no test"));
        assert!(problems[1].contains("no_such_test is not a test function"));
        let fine: &[(&str, Coverage)] = &[
            ("A_ROW", Test("real_test")),
            ("B_ROW", Coverage::Untestable("needs a clock")),
        ];
        assert!(coverage_problems(fine, &names, &tests).is_empty());
        let stray: &[(&str, Coverage)] = &[
            ("A_ROW", Pending(384)),
            ("B_ROW", Pending(384)),
            ("C", Pending(1)),
        ];
        assert_eq!(coverage_problems(stray, &names, &tests).len(), 1);
    }

    #[test]
    fn a_function_that_is_not_a_test_does_not_cover_a_row() {
        let text = "fn helper() {}\n\n#[test]\n#[ignore = \"x\"]\nfn real() {}\n\
                    #[sqlx::test]\nasync fn db(pool: PgPool) {}\n#[tokio::test]\nasync fn rt() {}\n\
                    #[test]\nfn first() {}\nfn after() {}\n";

        let names = test_names_in(text);

        assert_eq!(names, ["real", "db", "rt", "first"]);
        let tests: HashSet<String> = names.into_iter().collect();
        let table: &[(&str, Coverage)] = &[("A_ROW", Test("helper")), ("B_ROW", Test("real"))];
        let found = coverage_problems(table, &["A_ROW", "B_ROW"], &tests);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("helper is not a test function"));
    }

    #[test]
    fn the_pending_rows_are_all_for_384() {
        for (row, coverage) in COVERAGE {
            if let Coverage::Pending(issue) = coverage {
                assert_eq!(*issue, 384, "{row}");
            }
        }
    }

    fn actions_fixture() -> Value {
        json!({ "actions": decision_actions::actions() })
    }

    fn actions_fixture_path() -> PathBuf {
        crates_dir().join("../web/src/features/traces/fixtures/decision-actions.json")
    }

    #[test]
    fn the_web_action_list_is_the_registry() {
        let wanted = actions_fixture();
        if std::env::var_os("ZIRV_FLOW_BLESS").is_some() {
            let mut text = serde_json::to_string_pretty(&wanted).expect("the list serializes");
            text.push('\n');
            std::fs::write(actions_fixture_path(), text).expect("the list is written");
            return;
        }
        let text = std::fs::read_to_string(actions_fixture_path())
            .expect("the action list exists; regenerate it with ZIRV_FLOW_BLESS=1");
        let committed: Value = serde_json::from_str(&text).expect("the action list is JSON");

        assert_eq!(
            committed, wanted,
            "the action list differs from the registry; regenerate it with ZIRV_FLOW_BLESS=1 \
             cargo nextest run -p zirv-flow decisions and review the diff"
        );
    }

    #[test]
    fn every_row_is_a_spec_with_its_own_constant() {
        let _: &[&DecisionSpec] = decision_actions::ALL;
        assert_eq!(decision_actions::ALL.len(), COVERAGE.len());
    }
}
