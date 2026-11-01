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
    const SPEC_BUILDERS: [&str; 1] = ["crates/zirv-flow-core/src/decision_actions.rs"];

    /// The only production files that insert into the decision log with SQL of their own; each
    /// binds its action and outcome from the registry.
    const SQL_WRITERS: [&str; 3] = [
        "crates/zirv-flow-store/src/decisions.rs",
        "crates/zirv-flow-store/src/jobs.rs",
        "crates/zirv-flow-store/src/segments/refresh.rs",
    ];

    /// What produces a registry row in a test.
    #[derive(Clone, Copy, Debug)]
    enum Coverage {
        /// The name of a test that makes the decision and checks it.
        Test(&'static str),
        /// No test can make it, and why.
        Untestable(&'static str),
    }

    use Coverage::Test;

    const SEND_PATH: &str = "the_send_path_and_its_trace_hold_end_to_end";

    /// Every registry row, by constant name, with what covers it.
    #[rustfmt::skip]
    const COVERAGE: &[(&str, Coverage)] = &[
        ("EVENT_ACCEPTED_VALID", Test(SEND_PATH)),
        ("EVENT_ACCEPTED_FLAGGED", Test("event_accepted_flagged")),
        ("EVENT_ACCEPTED_UNPLANNED", Test("event_accepted_unplanned")),
        ("EVENT_REJECTED", Test("decisions_name_ids_and_codes_only")),
        ("RECORD_REJECTED_APPLY_FAILED", Test("an_unmappable_identify_row_becomes_a_record_reject")),
        ("EVENT_REPLAYED_VALID", Test("a_replay_after_a_schema_fix_stores_the_event_once_and_a_second_changes_nothing")),
        ("EVENT_REPLAYED_FLAGGED", Test("event_replayed_flagged")),
        ("EVENT_REPLAYED_UNPLANNED", Test("event_replayed_unplanned")),
        ("PERSON_CREATED", Test("the_first_record_creates_a_person_with_identities_traits_and_decisions")),
        ("TRAIT_CHANGED", Test("the_first_record_creates_a_person_with_identities_traits_and_decisions")),
        ("TRAIT_CHANGED_DEFINITION", Test("a_precedence_change_is_applied_to_existing_people_by_the_batch")),
        ("TRAIT_CHANGED_MERGED", Test("an_anonymous_person_is_merged_into_the_identified_one")),
        ("CONTRIBUTION_REMOVED", Test("removing_a_contribution_deletes_only_the_identities_no_other_record_lists")),
        ("IDENTITY_SPLIT", Test("a_split_with_no_matching_owner_is_recorded")),
        ("IDENTITY_LIMIT", Test("identifiers_beyond_the_maximum_of_a_kind_are_skipped_and_the_rest_applies")),
        ("IDENTITY_CONFLICT_RECORDED", Test("a_merge_the_rules_forbid_attaches_to_the_strongest_identifier_and_records_a_conflict")),
        ("IDENTITY_CONFLICT_RESOLVED_KEPT_SEPARATE", Test("keeping_two_people_separate_closes_the_conflict_and_changes_nothing_else")),
        ("IDENTITY_CONFLICT_RESOLVED_IDENTIFIER_MOVED", Test("moving_the_identifier_gives_it_to_the_chosen_person_and_audits_ids_only")),
        ("PERSON_MERGED", Test("an_anonymous_person_is_merged_into_the_identified_one")),
        ("PERSON_MERGE_REWRITE_COMPLETED", Test("the_rewrite_re_points_in_batches_and_a_second_run_moves_nothing")),
        ("PERSON_DELETED", Test("a_deletion_adds_no_suppression_and_records_its_reason_without_a_person")),
        ("CONSENT_CHANGED", Test(SEND_PATH)),
        ("CONSENT_SUPERSEDED", Test("consent_superseded")),
        ("CONSENT_POLICY_PUBLISHED", Test("a_published_version_is_immutable_and_each_publish_is_a_definition_change_decision")),
        ("LIST_MEMBERSHIP_ADDED", Test("decisions_are_written_for_real_changes_only_and_hold_ids")),
        ("LIST_MEMBERSHIP_REMOVED", Test("decisions_are_written_for_real_changes_only_and_hold_ids")),
        ("LIST_MEMBERSHIP_MOVED", Test("a_merge_leaves_the_survivor_in_the_union_of_both_peoples_lists")),
        ("TEMPLATE_PUBLISHED", Test("publish_records_a_definition_change_decision_and_the_validation")),
        ("SENDING_DOMAIN_VERIFIED", Test("a_failed_recheck_drops_a_verified_domain_and_appends_a_decision")),
        ("SENDING_DOMAIN_UNVERIFIED", Test("a_failed_recheck_drops_a_verified_domain_and_appends_a_decision")),
        ("FLOW_PUBLISHED", Test("the_lifecycle_moves_through_its_states_and_records_a_decision_for_each")),
        ("FLOW_PAUSED", Test("the_lifecycle_moves_through_its_states_and_records_a_decision_for_each")),
        ("FLOW_RESUMED", Test("the_lifecycle_moves_through_its_states_and_records_a_decision_for_each")),
        ("FLOW_ARCHIVED", Test("the_lifecycle_moves_through_its_states_and_records_a_decision_for_each")),
        ("FLOW_ENROLLED", Test(SEND_PATH)),
        ("FLOW_ENROLL_SKIPPED", Test("once_refuses_a_person_who_entered_before_even_after_the_enrollment_ended")),
        ("FLOW_ENROLL_DEFERRED", Test("a_published_flow_spreads_a_backlog_over_its_window")),
        ("FLOW_ENROLLMENT_COMPLETED", Test(SEND_PATH)),
        ("FLOW_ENROLLMENT_EXITED", Test("an_exit_works_in_batches_and_leaves_ended_enrollments_alone")),
        ("FLOW_ENROLLMENT_FAILED", Test("a_step_that_keeps_failing_is_retried_and_ends_failed_with_its_error_visible")),
        ("FLOW_ENROLLMENT_HELD", Test("a_paused_flow_holds_a_due_timer_and_resume_runs_it_once")),
        ("FLOW_ENROLLMENT_RESUMED", Test("a_paused_flow_holds_a_due_timer_and_resume_runs_it_once")),
        ("FLOW_ENROLLMENT_RETRIED", Test("a_segment_triggered_enrollment_that_fails_is_retried_and_skipped_on_one_trace")),
        ("FLOW_ENROLLMENT_SKIPPED", Test("a_segment_triggered_enrollment_that_fails_is_retried_and_skipped_on_one_trace")),
        ("FLOW_STEP_DONE", Test(SEND_PATH)),
        ("FLOW_STEP_ENQUEUED", Test(SEND_PATH)),
        ("FLOW_STEP_WAITING", Test(SEND_PATH)),
        ("FLOW_STEP_FAILED", Test("flow_step_failed")),
        ("FLOW_WAKE_EVENT", Test("an_event_that_commits_before_the_timeout_takes_the_event_path_and_the_timer_is_stale")),
        ("FLOW_WAKE_TIMEOUT", Test("a_timeout_that_commits_before_the_event_takes_the_timeout_path_and_the_event_is_stale")),
        ("FLOW_WAKE_TIMER", Test(SEND_PATH)),
        ("FLOW_WAIT_UNTIL_EVENT", Test("an_event_that_commits_before_the_timeout_takes_the_event_path_and_the_timer_is_stale")),
        ("FLOW_WAIT_UNTIL_TIME", Test("a_wait_until_a_time_falls_back_to_the_workspace_zone_and_says_so")),
        ("FLOW_WAIT_DURATION", Test("the_decisions_of_a_three_node_path_are_complete_and_on_the_enrollments_trace")),
        ("FLOW_GOAL_CONVERTED", Test("a_goal_converts_once_and_leaves_the_enrollment_running")),
        ("FLOW_BRANCH_CASE_0", Test("a_branch_sees_the_internal_effect_applied_earlier_in_the_same_step")),
        ("FLOW_BRANCH_CASE_1", Test("flow_branch_case_1")),
        ("FLOW_BRANCH_CASE_2", Test("flow_branch_case_2")),
        ("FLOW_BRANCH_CASE_3", Test("flow_branch_case_3")),
        ("FLOW_BRANCH_CASE_4", Test("flow_branch_case_4")),
        ("FLOW_BRANCH_CASE_5", Test("flow_branch_case_5")),
        ("FLOW_BRANCH_CASE_6", Test("flow_branch_case_6")),
        ("FLOW_BRANCH_CASE_7", Test("flow_branch_case_7")),
        ("FLOW_BRANCH_CASE_8", Test("flow_branch_case_8")),
        ("FLOW_BRANCH_CASE_9", Test("flow_branch_case_9")),
        ("FLOW_BRANCH_CASE_MORE", Test("flow_branch_case_more")),
        ("FLOW_BRANCH_ELSE", Test("flow_branch_else")),
        ("FLOW_BRANCH_CONDITION_MATCHED", Test("a_branch_sees_the_internal_effect_applied_earlier_in_the_same_step")),
        ("FLOW_BRANCH_CONDITION_UNMATCHED", Test("flow_branch_else")),
        ("SEND_GATE_PASSED", Test(SEND_PATH)),
        ("SEND_GATE_BLOCKED", Test(SEND_PATH)),
        ("MESSAGE_SEND_SENT", Test(SEND_PATH)),
        ("MESSAGE_SEND_SKIPPED", Test(SEND_PATH)),
        ("MESSAGE_SEND_FAILED", Test("message_send_retry_exhausted")),
        ("MESSAGE_SEND_DROPPED", Test("a_send_for_an_ended_enrollment_or_a_paused_flow_is_dropped_with_a_decision_only")),
        ("MESSAGE_SEND_DEFERRED", Test("a_send_that_finds_no_token_runs_again_when_the_bucket_will_hold_one")),
        ("MESSAGE_SEND_NOTED", Test("a_person_zone_overrides_the_workspace_zone_and_an_invalid_one_falls_back_and_says_so")),
        ("MESSAGE_FEEDBACK_DELIVERED", Test(SEND_PATH)),
        ("MESSAGE_FEEDBACK_BOUNCED_HARD", Test("message_feedback_bounced_hard")),
        ("MESSAGE_FEEDBACK_BOUNCED_SOFT", Test("message_feedback_bounced_soft")),
        ("MESSAGE_FEEDBACK_COMPLAINED", Test("message_feedback_complained")),
        ("SUPPRESSION_FEEDBACK_ORGANIZATION", Test("message_feedback_bounced_hard")),
        ("SUPPRESSION_FEEDBACK_WORKSPACE", Test("message_feedback_complained")),
        ("SUPPRESSION_FEEDBACK_UNABLE", Test("a_message_without_a_stored_hash_writes_the_event_but_suppresses_nobody")),
        ("WEBHOOK_CALL_SUCCEEDED", Test("an_outcome_writes_the_event_and_the_decision_and_wakes_a_waiting_enrollment")),
        ("WEBHOOK_CALL_FAILED", Test("a_failure_wakes_only_a_wait_for_the_failed_event")),
        ("SYNC_UPSERT_SUCCEEDED", Test("an_upsert_sends_exactly_the_listed_traits_under_the_key_and_records_its_outcome")),
        ("SYNC_UPSERT_FAILED", Test("a_person_without_the_key_ends_missing_key_with_no_call_and_no_attempt")),
        ("SYNC_REMOVE_SUCCEEDED", Test("a_remove_deletes_the_profile_under_the_same_key")),
        ("SYNC_REMOVE_FAILED", Test("a_4xx_is_terminal_and_the_last_attempt_of_a_5xx_is_recorded_as_a_failure")),
        ("EVENT_FORWARD_RETRY", Test("a_retry_reuses_the_key_backs_off_and_leaves_a_job_for_the_next_attempt")),
        ("EVENT_FORWARD_DEAD", Test("a_terminal_answer_is_dead_at_once_and_a_redrive_delivers_it_without_a_flow")),
        ("SYNC_RECORD_APPLIED", Test("a_batch_applies_deletes_and_rejects_and_records_each_under_the_run_trace")),
        ("SYNC_RECORD_DELETED", Test("a_removed_record_appends_a_deleted_decision_with_the_reason_it_was_removed_for")),
        ("SYNC_RECORD_REJECTED", Test("a_batch_applies_deletes_and_rejects_and_records_each_under_the_run_trace")),
        ("SEGMENT_ENTERED", Test(SEND_PATH)),
        ("SEGMENT_EXITED", Test("a_refresh_enters_and_exits_people_with_events_decisions_and_counts")),
        ("SEGMENT_REFRESH_SKIPPED", Test("an_archived_segment_is_skipped_with_a_decision_and_changes_nothing")),
        ("SEGMENT_REFRESH_FAILED", Test("a_definition_that_no_longer_compiles_leaves_the_members_and_records_a_failure")),
        ("SEGMENT_REEVALUATE_FAILED", Test("a_definition_that_no_longer_compiles_is_recorded_and_the_others_still_commit")),
        ("JOB_ATTEMPT_FAILED", Test("a_failed_workspace_job_leaves_a_job_attempt_decision_and_an_instance_job_none")),
        ("JOB_ATTEMPT_EXHAUSTED", Test("a_failed_workspace_job_leaves_a_job_attempt_decision_and_an_instance_job_none")),
        ("JOB_ATTEMPT_LEASE_EXPIRED", Test("a_reaped_workspace_job_leaves_a_lease_expired_or_exhausted_decision")),
        ("JOB_RETRY_REQUEUED", Test("retrying_a_dead_workspace_job_requeues_it_under_its_trace_and_audits_and_records_it")),
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

    const DECISION_ACCEPTANCE_SUITES: &[&str] = &[
        "crates/zirv-flow/src/e2e/trace.rs",
        "crates/zirv-flow-api/src/flows/acceptance.rs",
        "crates/zirv-flow-jobs/src/kinds/delivery_send/mod.rs",
        "crates/zirv-flow-jobs/src/kinds/delivery_sync.rs",
        "crates/zirv-flow-jobs/src/kinds/event_forward.rs",
        "crates/zirv-flow-jobs/src/kinds/flow_enroll.rs",
        "crates/zirv-flow-jobs/src/kinds/flow_step.rs",
        "crates/zirv-flow-jobs/src/kinds/import_run.rs",
        "crates/zirv-flow-jobs/src/kinds/ingest_drain.rs",
        "crates/zirv-flow-jobs/src/kinds/privacy_erase.rs",
        "crates/zirv-flow-jobs/src/kinds/privacy_export.rs",
        "crates/zirv-flow-jobs/src/kinds/segment_reevaluate.rs",
        "crates/zirv-flow-jobs/src/kinds/segment_refresh.rs",
        "crates/zirv-flow-jobs/src/kinds/sync_run.rs",
        "crates/zirv-flow-store/src/consent/state.rs",
        "crates/zirv-flow-store/src/feedback.rs",
        "crates/zirv-flow-store/src/forwarding/outbox.rs",
        "crates/zirv-flow-store/src/imports/job.rs",
        "crates/zirv-flow-store/src/ingest.rs",
        "crates/zirv-flow-store/src/privacy/delete.rs",
        "crates/zirv-flow-store/src/privacy/erasure.rs",
        "crates/zirv-flow-store/src/privacy/export.rs",
        "crates/zirv-flow-store/src/segments/reevaluate.rs",
        "crates/zirv-flow-store/src/segments/refresh.rs",
        "crates/zirv-flow-store/src/sync_runs/batch.rs",
        "crates/zirv-flow/src/e2e/mod.rs",
        "crates/zirv-flow/src/e2e/propagation.rs",
    ];

    fn checked_test_body(body: &str, pool: &str) -> bool {
        let Some(open) = body.find('{') else {
            return false;
        };
        let prefix: String = body[..open].split_whitespace().collect();
        if !["crate", "zirv_flow_store"].iter().any(|path| {
            prefix == format!("{path}::decision_testing::checked({pool},|{pool}|asyncmove")
        }) {
            return false;
        }
        let end = after_block(body, open);
        body[end..].split_whitespace().collect::<String>() == ").await;"
    }

    fn unchecked_database_tests(mut source: &str) -> Vec<&str> {
        let mut unchecked = Vec::new();
        while let Some(attribute) = source.find("#[sqlx::test") {
            let function = source[attribute..]
                .split_once("fn ")
                .expect("a database test is a function")
                .1;
            let (name, signature) = function.split_once('(').expect("a test has parameters");
            let (parameters, body) = signature.split_once(')').expect("parameters end");
            let (pool, _) = parameters.split_once(':').expect("a test takes a pool");
            let open = body.find('{').expect("a test has a body");
            let end = after_block(body, open);
            if !checked_test_body(&body[open + 1..end - 1], pool.trim()) {
                unchecked.push(name.trim());
            }
            source = &body[end..];
        }
        unchecked
    }

    #[test]
    fn decision_acceptance_suites_keep_the_invariant_on_new_database_tests() {
        for file in DECISION_ACCEPTANCE_SUITES {
            let source = std::fs::read_to_string(workspace_dir().join(file))
                .unwrap_or_else(|error| panic!("{file}: {error}"));
            assert!(source.contains("#[sqlx::test"), "{file}: no database tests");
            let unchecked = unchecked_database_tests(&source);
            assert!(
                unchecked.is_empty(),
                "{file}: each database test must contain only decision_testing::checked(...).await;: {unchecked:?}"
            );
        }
    }

    #[test]
    fn the_invariant_scan_requires_the_wrapper_to_be_the_whole_test_body() {
        let source = r##"
            #[sqlx::test]
            #[ignore = "requires PostgreSQL; set DATABASE_URL"]
            async fn wrapped(pool: PgPool) {
                crate::decision_testing::checked(pool, |pool| async move {
                    let braces = r#"{ ) }.await; }"#;
                    /* } */
                    if braces.is_empty() { return; }
                }).await;
            }
            #[sqlx::test]
            async fn unwrapped(pool: PgPool) {
                write(&pool).await;
            }
            #[sqlx::test]
            async fn before(pool: PgPool) {
                write(&pool).await;
                crate::decision_testing::checked(pool, |pool| async move {}).await;
            }
            #[sqlx::test]
            async fn after(pool: PgPool) {
                crate::decision_testing::checked(pool, |pool| async move {}).await;
                write(&pool).await;
            }
            #[sqlx::test]
            async fn nested(pool: PgPool) {
                if true {
                    crate::decision_testing::checked(pool, |pool| async move {}).await;
                }
            }
        "##;
        for source in [
            source.to_owned(),
            source.replace("crate::", "zirv_flow_store::"),
        ] {
            assert_eq!(
                unchecked_database_tests(&source),
                ["unwrapped", "before", "after", "nested"]
            );
        }
    }

    /// Whether the byte before `at` continues an identifier, so a `b` or `r` at `at` is part of
    /// a name and not a literal prefix.
    fn after_identifier(bytes: &[u8], at: usize) -> bool {
        at > 0 && (bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_')
    }

    /// The index just past the string whose opening `"` is at `quote`, with `hashes` `#`s in
    /// its raw delimiter (`None` for a string with escapes).
    fn after_string(text: &str, quote: usize, hashes: Option<usize>) -> usize {
        let bytes = text.as_bytes();
        match hashes {
            Some(hashes) => {
                let close = format!("\"{}", "#".repeat(hashes));
                text[quote + 1..]
                    .find(&close)
                    .map_or(bytes.len(), |end| quote + 1 + end + close.len())
            }
            None => {
                let mut at = quote + 1;
                while at < bytes.len() && bytes[at] != b'"' {
                    at += if bytes[at] == b'\\' { 2 } else { 1 };
                }
                (at + 1).min(bytes.len())
            }
        }
    }

    /// The index just past the block comment that starts at `at`; they nest.
    fn after_block_comment(bytes: &[u8], at: usize) -> usize {
        let mut level = 1;
        let mut at = at + 2;
        while at < bytes.len() && level > 0 {
            if bytes[at..].starts_with(b"/*") {
                level += 1;
                at += 2;
            } else if bytes[at..].starts_with(b"*/") {
                level -= 1;
                at += 2;
            } else {
                at += 1;
            }
        }
        at.min(bytes.len())
    }

    /// The index just past the item whose `{` is the first one at or after `from`: the matching
    /// `}`. Nested block comments, strings, byte strings, raw strings with any number of `#`s,
    /// character and byte literals do not count their braces; a lifetime is not a literal.
    fn after_block(text: &str, from: usize) -> usize {
        let bytes = text.as_bytes();
        let mut depth = 0usize;
        let mut at = from;
        while at < bytes.len() {
            match bytes[at] {
                b'/' if bytes.get(at + 1) == Some(&b'/') => {
                    at += text[at..].find('\n').unwrap_or(bytes.len() - at);
                }
                b'/' if bytes.get(at + 1) == Some(&b'*') => at = after_block_comment(bytes, at),
                b'"' => at = after_string(text, at, None),
                b'b' | b'r' if !after_identifier(bytes, at) => {
                    let raw = if bytes[at] == b'b' { at + 1 } else { at };
                    let hashes = bytes
                        .get(raw + 1..)
                        .unwrap_or_default()
                        .iter()
                        .take_while(|b| **b == b'#')
                        .count();
                    if bytes.get(raw) == Some(&b'r') && bytes.get(raw + 1 + hashes) == Some(&b'"') {
                        at = after_string(text, raw + 1 + hashes, Some(hashes));
                    } else {
                        // `b"` and `b'` continue as the string or character that follows.
                        at += 1;
                    }
                }
                b'\'' => {
                    if bytes.get(at + 1) == Some(&b'\\') {
                        // An escaped character, such as '\'' or '\u{7d}'.
                        let body = at + 3;
                        at = text[body.min(bytes.len())..]
                            .find('\'')
                            .map_or(bytes.len(), |end| body + end + 1);
                    } else {
                        let width = text[at + 1..].chars().next().map_or(1, char::len_utf8);
                        if bytes.get(at + 1 + width) == Some(&b'\'') {
                            at += width + 2;
                        } else {
                            // A lifetime.
                            at += 1;
                        }
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

    fn workspace_dir() -> PathBuf {
        crates_dir().join("..")
    }

    /// The directories of the workspace members, from the `members` list of the root
    /// `Cargo.toml`; a `dir/*` entry stands for every subdirectory that holds a `Cargo.toml`.
    fn workspace_members() -> Vec<PathBuf> {
        let manifest = std::fs::read_to_string(workspace_dir().join("Cargo.toml"))
            .expect("the workspace manifest reads");
        let after = manifest
            .split_once("members")
            .expect("the workspace lists members")
            .1;
        let list = after
            .split_once('[')
            .and_then(|(_, rest)| rest.split_once(']'))
            .expect("the members are a list")
            .0;
        let mut members = Vec::new();
        for entry in list.split('"').skip(1).step_by(2) {
            match entry.strip_suffix("/*") {
                Some(parent) => {
                    let mut found: Vec<PathBuf> = std::fs::read_dir(workspace_dir().join(parent))
                        .expect("a members directory reads")
                        .map(|entry| entry.expect("a directory entry reads").path())
                        .filter(|path| path.join("Cargo.toml").is_file())
                        .collect();
                    found.sort();
                    members.extend(found);
                }
                None => members.push(workspace_dir().join(entry)),
            }
        }
        members
    }

    /// The production source of every workspace member, its `build.rs` included: (path relative
    /// to the workspace root, text). A file a parent declares as `#[cfg(test)] mod name;` is
    /// test code and is left out.
    fn production_sources() -> Vec<(String, String)> {
        let mut found = Vec::new();
        for member in workspace_members() {
            rust_files(&member.join("src"), &mut found);
            let build = member.join("build.rs");
            if build.is_file() {
                found.push(build);
            }
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
                    .strip_prefix(workspace_dir())
                    .expect("the file is under the workspace")
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

    /// `text` without its SQL comments: `--` to the end of the line (or to a literal `\n` in a
    /// Rust string) and `/* .. */`, which nests. A comment that never closes is kept.
    fn without_sql_comments(text: &str) -> String {
        let bytes = text.as_bytes();
        let mut kept = String::with_capacity(text.len());
        let mut at = 0;
        let mut from = 0;
        while at < bytes.len() {
            let end = if bytes[at..].starts_with(b"--") {
                let line = text[at..].find('\n').unwrap_or(bytes.len() - at);
                let escaped = text[at..].find("\\n").unwrap_or(bytes.len() - at);
                Some(at + line.min(escaped))
            } else if bytes[at..].starts_with(b"/*") {
                let end = after_block_comment(bytes, at);
                bytes[..end].ends_with(b"*/").then_some(end)
            } else {
                None
            };
            match end {
                Some(end) => {
                    kept.push_str(&text[from..at]);
                    kept.push(' ');
                    at = end;
                    from = end;
                }
                None => at += 1,
            }
        }
        kept.push_str(&text[from..]);
        kept
    }

    /// `text` in lower case with quotes and backslashes dropped, runs of white space as one
    /// space, and no space around a dot.
    fn normalised_sql(text: &str) -> String {
        let flat: String = text
            .to_ascii_lowercase()
            .chars()
            .filter(|c| *c != '"' && *c != '\\')
            .collect();
        flat.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .replace(" . ", ".")
            .replace(" .", ".")
            .replace(". ", ".")
    }

    /// Whether `text` holds an `INSERT INTO decision_records`, whatever its case, spacing,
    /// quoting, comments in between or `public.` prefix.
    fn inserts_into_the_log(text: &str) -> bool {
        [text.to_owned(), without_sql_comments(text)]
            .iter()
            .map(|candidate| normalised_sql(candidate))
            .any(|sql| {
                sql.contains("insert into decision_records")
                    || sql.contains("insert into public.decision_records")
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
                problems.push(format!("{name} has no test or Untestable entry"));
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
                "crates/zirv-flow-store/src/jobs.rs",
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
    fn the_scan_still_reads_a_writer_after_each_literal_a_test_module_can_hold() {
        let writer = "fn later() { sqlx::query(\"INSERT INTO decision_records (id)\"); }\n";
        let constructs = [
            "/* /* */ } */",
            "/* /* } */ */",
            "let s = br##\"} \"# }\"##;",
            "let s = br\"}\";",
            "let s = r##\"\"# }\"##;",
            "let s = b\"}\";",
            "let c = b'}';",
            "let c = '}';",
            "let c = '\\'';",
            "let c = '\\u{7d}';",
            "fn f<'a>(x: &'a str) -> &'a str { x }",
            "let s = \"\\\"}\";",
            "// }",
        ];
        for construct in constructs {
            let text = format!(
                "#[cfg(test)]\nmod tests {{\n    fn f() {{
{construct}
    }}\n}}\n{writer}"
            );

            let found = problems("zirv-flow-api/src/x.rs", &production(&text));

            assert_eq!(
                found,
                ["zirv-flow-api/src/x.rs: writes decision_records outside the registry"],
                "{construct}"
            );
        }
    }

    #[test]
    fn the_scan_drops_a_test_module_that_holds_each_literal() {
        let text = "#[cfg(test)]\nmod tests {\n    fn f() { let s = br#\"}\"#; /* /* */ } */ let c = '}'; }\n    fn g() { INSERT INTO decision_records }\n}\nfn after() {}\n";

        assert_eq!(production(text), "fn after() {}\n");
    }

    #[test]
    fn the_scan_catches_an_insert_with_spaced_dots_quotes_and_comments() {
        let spellings = [
            "INSERT INTO public . decision_records (id)",
            "INSERT INTO \"public\" . \"decision_records\" (id)",
            "INSERT /* a */ INTO -- b\n decision_records (id)",
            "insert /* a /* nested */ b */ into decision_records",
            "insert into public.\n  decision_records",
            "-- note\\nINSERT INTO decision_records (id)",
            "INSERT INTO \\\"decision_records\\\" (id)",
        ];
        for sql in spellings {
            assert!(inserts_into_the_log(sql), "{sql}");
        }
        assert!(!inserts_into_the_log(
            "INSERT INTO public.audit_entries (id)"
        ));
    }

    #[test]
    fn the_workspace_members_come_from_the_manifest() {
        let members: Vec<String> = workspace_members()
            .iter()
            .map(|member| {
                let relative = member
                    .strip_prefix(workspace_dir())
                    .expect("under the root");
                relative.to_string_lossy().replace('\\', "/")
            })
            .collect();

        assert!(
            members.contains(&"crates/zirv-flow-store".to_owned()),
            "{members:?}"
        );
        assert!(members.contains(&"tools/loadgen".to_owned()), "{members:?}");
        assert!(
            members.contains(&"tools/renderbench".to_owned()),
            "{members:?}"
        );
        let sources = production_sources();
        for expected in [
            "crates/zirv-flow-store/build.rs",
            "crates/zirv-flow-api/build.rs",
            "tools/loadgen/src/main.rs",
        ] {
            assert!(
                sources.iter().any(|(path, _)| path == expected),
                "{expected}"
            );
        }
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
            ("A_ROW", Test("real_test")),
            ("B_ROW", Test("real_test")),
            ("C", Test("real_test")),
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
    fn every_registry_row_has_a_producing_test() {
        for (row, coverage) in COVERAGE {
            assert!(matches!(coverage, Coverage::Test(_)), "{row}: {coverage:?}");
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
