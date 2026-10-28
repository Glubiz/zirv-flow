//! The decision actions (architecture section 12.4): every (action, outcome, reason) the decision
//! log can hold, as one table.
//!
//! A decision record is written from a [`DecisionSpec`], never from three loose strings. A spec
//! has private fields and is built only in this module, so outside this crate's tests a triple
//! that is not in [`ALL`] does not compile. The audit catalogue ([`crate::audit`]) is a
//! separate registry for a different log.
//!
//! The reason of a row is one of three things: none, a fixed code, or a detail (see [`Reason`]).
//! A detail row stands for an outcome whose reason is a code that the code that decided
//! chose at run time, such as a gate name or a provider error class.

use crate::CauseKind;

/// How a decision's reason is set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reason {
    /// The record has no reason.
    None,
    /// The record always carries this reason code.
    Fixed(&'static str),
    /// The record carries a reason the deciding code picked at run time, from a closed set it
    /// owns (a gate, a refusal code, a source) as a static code. The text names what
    /// the reason ranges over. Never a value from a person's data.
    Detail(&'static str),
}

/// One row of the registry: a decision the system can write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecisionSpec {
    action: &'static str,
    outcome: &'static str,
    reason: Reason,
    causes: &'static [CauseKind],
    subjects: &'static [&'static str],
}

impl DecisionSpec {
    /// A row outside the registry, for tests of other crates that need an arbitrary action.
    /// Behind the `test-support` feature, which only dev-dependencies enable.
    #[cfg(feature = "test-support")]
    pub const fn for_test(
        action: &'static str,
        outcome: &'static str,
        reason: Reason,
        causes: &'static [CauseKind],
        subjects: &'static [&'static str],
    ) -> Self {
        Self {
            action,
            outcome,
            reason,
            causes,
            subjects,
        }
    }

    /// The action, such as `flow.step`.
    pub const fn action(&self) -> &'static str {
        self.action
    }

    /// The outcome of the action.
    pub const fn outcome(&self) -> &'static str {
        self.outcome
    }

    /// How the reason is set.
    pub const fn reason(&self) -> Reason {
        self.reason
    }

    /// The kinds of cause the decision is written under.
    pub const fn causes(&self) -> &'static [CauseKind] {
        self.causes
    }

    /// The names of the durable ids its `subject_ids` may hold. A flow decision also carries the
    /// graph point, which is not a subject.
    pub const fn subjects(&self) -> &'static [&'static str] {
        self.subjects
    }

    /// The reason a record of this row stores: the fixed code, else the detail the caller gave.
    /// A row without a detail reason ignores `detail`.
    pub const fn reason_text(&self, detail: Option<&'static str>) -> Option<&'static str> {
        match self.reason {
            Reason::None => None,
            Reason::Fixed(code) => Some(code),
            Reason::Detail(_) => detail,
        }
    }
}

/// Every cause kind.
const ANY: &[CauseKind] = &[
    CauseKind::Event,
    CauseKind::Schedule,
    CauseKind::Manual,
    CauseKind::Api,
    CauseKind::DefinitionChange,
    CauseKind::SyncRun,
];
/// Work that arrives as an API request or a sync run.
const INGEST: &[CauseKind] = &[CauseKind::Api, CauseKind::SyncRun];
/// A change a person makes through the API or the admin command line.
const OPERATOR: &[CauseKind] = &[CauseKind::Api, CauseKind::Manual];
const API: &[CauseKind] = &[CauseKind::Api];
const SYNC: &[CauseKind] = &[CauseKind::SyncRun];
const DEFINITION: &[CauseKind] = &[CauseKind::DefinitionChange];

const FLOW: &[&str] = &["flow", "enrollment", "person", "step_execution"];
const MESSAGE: &[&str] = &[
    "message",
    "enrollment",
    "person",
    "template",
    "destination",
    "event",
    "flow",
];
const SEND_GATE: &[&str] = &[
    "message",
    "enrollment",
    "person",
    "template",
    "destination",
    "event",
    "flow",
    "change",
    "rule",
];
const CALL: &[&str] = &["enrollment", "person", "event", "flow", "destination"];
const FORWARD: &[&str] = &["destination", "rule", "event", "outbox"];

macro_rules! registry {
    ($($name:ident: $action:literal, $outcome:literal, $reason:expr, $causes:expr, $subjects:expr;)+) => {
        $(
            pub const $name: DecisionSpec = DecisionSpec {
                action: $action,
                outcome: $outcome,
                reason: $reason,
                causes: $causes,
                subjects: $subjects,
            };
        )+

        /// Every row.
        pub const ALL: &[&DecisionSpec] = &[$(&$name),+];

        /// The constant name of each row, in the order of [`ALL`].
        pub const NAMES: &[&str] = &[$(stringify!($name)),+];
    };
}

registry! {
    EVENT_ACCEPTED_VALID: "event.accepted", "valid", Reason::None, INGEST, &["event", "person"];
    EVENT_ACCEPTED_FLAGGED: "event.accepted", "flagged", Reason::None, INGEST, &["event", "person"];
    EVENT_ACCEPTED_UNPLANNED: "event.accepted", "unplanned", Reason::None, INGEST, &["event", "person"];
    EVENT_REJECTED: "event.rejected", "rejected", Reason::Detail("the refusal code"), INGEST, &["reject", "person"];
    RECORD_REJECTED_APPLY_FAILED: "record.rejected", "rejected", Reason::Fixed("apply_failed"), INGEST, &["reject"];
    EVENT_REPLAYED_VALID: "event.replayed", "valid", Reason::None, OPERATOR, &["reject", "event", "person"];
    EVENT_REPLAYED_FLAGGED: "event.replayed", "flagged", Reason::None, OPERATOR, &["reject", "event", "person"];
    EVENT_REPLAYED_UNPLANNED: "event.replayed", "unplanned", Reason::None, OPERATOR, &["reject", "event", "person"];

    PERSON_CREATED: "person.created", "created", Reason::None, INGEST, &["person", "source_record"];
    TRAIT_CHANGED: "trait.change", "changed", Reason::None, INGEST, &["person", "source_record"];
    TRAIT_CHANGED_DEFINITION: "trait.change", "changed", Reason::Fixed("definition_changed"), DEFINITION, &["person"];
    TRAIT_CHANGED_MERGED: "trait.change", "changed", Reason::Fixed("merged"), INGEST, &["person"];
    CONTRIBUTION_REMOVED: "contribution.removed", "removed", Reason::None, INGEST, &["person", "source_record"];
    IDENTITY_SPLIT: "identity.split", "new_person", Reason::Fixed("unique_kind"), INGEST, &["person", "previous_person", "source_record"];
    IDENTITY_LIMIT: "identity.limit", "skipped", Reason::Fixed("max_per_person"), INGEST, &["person", "source_record"];
    IDENTITY_CONFLICT_RECORDED: "identity.conflict", "recorded", Reason::Detail("the conflict kind"), INGEST, &["person", "other_person", "source_record"];
    IDENTITY_CONFLICT_RESOLVED_KEPT_SEPARATE: "identity.conflict.resolved", "kept_separate", Reason::None, API, &["conflict", "person", "other_person"];
    IDENTITY_CONFLICT_RESOLVED_IDENTIFIER_MOVED: "identity.conflict.resolved", "identifier_moved", Reason::None, API, &["conflict", "person", "other_person"];
    PERSON_MERGED: "person.merge", "merged", Reason::None, INGEST, &["person", "survivor", "merged", "merge"];
    PERSON_MERGE_REWRITE_COMPLETED: "person.merge.rewrite", "completed", Reason::None, INGEST, &["person", "survivor", "merged"];
    PERSON_DELETED: "person.deleted", "deleted", Reason::Detail("the erasure reason"), OPERATOR, &[];

    CONSENT_CHANGED: "consent.change", "changed", Reason::Detail("the consent source"), ANY, &["person", "consent_change"];
    CONSENT_SUPERSEDED: "consent.change", "superseded", Reason::Detail("the consent source"), ANY, &["person", "consent_change"];
    CONSENT_POLICY_PUBLISHED: "consent_policy.publish", "published", Reason::None, API, &["consent_policy"];
    LIST_MEMBERSHIP_ADDED: "list.membership", "added", Reason::Detail("the list source"), ANY, &["list", "person"];
    LIST_MEMBERSHIP_REMOVED: "list.membership", "removed", Reason::Detail("the list source"), ANY, &["list", "person"];
    LIST_MEMBERSHIP_MOVED: "list.membership", "moved", Reason::Fixed("merge"), INGEST, &["person", "survivor", "merged"];
    TEMPLATE_PUBLISHED: "template.publish", "published", Reason::None, API, &["template"];
    SENDING_DOMAIN_VERIFIED: "sending_domain.verify", "verified", Reason::Fixed("records_ok"), ANY, &["sending_domain"];
    SENDING_DOMAIN_UNVERIFIED: "sending_domain.verify", "unverified", Reason::Fixed("record_failed"), ANY, &["sending_domain"];

    FLOW_PUBLISHED: "flow.publish", "published", Reason::None, OPERATOR, &["flow"];
    FLOW_PAUSED: "flow.pause", "paused", Reason::None, OPERATOR, &["flow"];
    FLOW_RESUMED: "flow.resume", "resumed", Reason::None, OPERATOR, &["flow"];
    FLOW_ARCHIVED: "flow.archive", "archived", Reason::None, OPERATOR, &["flow"];
    FLOW_ENROLLED: "flow.enroll", "enrolled", Reason::None, ANY, &["flow", "person", "enrollment"];
    FLOW_ENROLL_SKIPPED: "flow.enroll", "skipped", Reason::Detail("why the person was not enrolled"), ANY, &["flow", "person"];
    FLOW_ENROLLMENT_COMPLETED: "flow.enrollment", "completed", Reason::None, ANY, FLOW;
    FLOW_ENROLLMENT_EXITED: "flow.enrollment", "exited", Reason::Detail("why the enrollment ended"), ANY, FLOW;
    FLOW_ENROLLMENT_FAILED: "flow.enrollment", "failed", Reason::Detail("the failure code"), ANY, FLOW;
    FLOW_ENROLLMENT_HELD: "flow.enrollment", "held", Reason::Fixed("flow_paused"), ANY, FLOW;
    FLOW_ENROLLMENT_RESUMED: "flow.enrollment", "resumed", Reason::Fixed("flow_resumed"), ANY, FLOW;
    FLOW_ENROLLMENT_RETRIED: "flow.enrollment", "retried", Reason::Fixed("operator"), OPERATOR, FLOW;
    FLOW_ENROLLMENT_SKIPPED: "flow.enrollment", "skipped", Reason::Fixed("operator"), OPERATOR, FLOW;
    FLOW_STEP_DONE: "flow.step", "done", Reason::Detail("the node type"), ANY, FLOW;
    FLOW_STEP_ENQUEUED: "flow.step", "enqueued", Reason::Detail("the kind of work queued"), ANY, FLOW;
    FLOW_STEP_WAITING: "flow.step", "waiting", Reason::Detail("the node type, or the time zone fallback"), ANY, FLOW;
    FLOW_STEP_FAILED: "flow.step", "failed", Reason::Detail("the failure code"), ANY, FLOW;
    FLOW_WAKE_EVENT: "flow.wake", "event", Reason::None, ANY, FLOW;
    FLOW_WAKE_TIMEOUT: "flow.wake", "timeout", Reason::None, ANY, FLOW;
    FLOW_WAKE_TIMER: "flow.wake", "timer", Reason::None, ANY, FLOW;
    FLOW_WAIT_UNTIL_EVENT: "flow.wait", "waiting", Reason::Fixed("until_event"), ANY, FLOW;
    FLOW_WAIT_UNTIL_TIME: "flow.wait", "waiting", Reason::Fixed("until_time"), ANY, FLOW;
    FLOW_WAIT_DURATION: "flow.wait", "waiting", Reason::Fixed("duration"), ANY, FLOW;
    FLOW_GOAL_CONVERTED: "flow.goal", "converted", Reason::None, ANY, FLOW;
    FLOW_BRANCH_CASE_0: "flow.branch", "case_0", Reason::None, ANY, FLOW;
    FLOW_BRANCH_CASE_1: "flow.branch", "case_1", Reason::None, ANY, FLOW;
    FLOW_BRANCH_CASE_2: "flow.branch", "case_2", Reason::None, ANY, FLOW;
    FLOW_BRANCH_CASE_3: "flow.branch", "case_3", Reason::None, ANY, FLOW;
    FLOW_BRANCH_CASE_4: "flow.branch", "case_4", Reason::None, ANY, FLOW;
    FLOW_BRANCH_CASE_5: "flow.branch", "case_5", Reason::None, ANY, FLOW;
    FLOW_BRANCH_CASE_6: "flow.branch", "case_6", Reason::None, ANY, FLOW;
    FLOW_BRANCH_CASE_7: "flow.branch", "case_7", Reason::None, ANY, FLOW;
    FLOW_BRANCH_CASE_8: "flow.branch", "case_8", Reason::None, ANY, FLOW;
    FLOW_BRANCH_CASE_9: "flow.branch", "case_9", Reason::None, ANY, FLOW;
    FLOW_BRANCH_CASE_MORE: "flow.branch", "case_more", Reason::None, ANY, FLOW;
    FLOW_BRANCH_ELSE: "flow.branch", "else", Reason::None, ANY, FLOW;
    FLOW_BRANCH_CONDITION_MATCHED: "flow.branch.condition", "matched", Reason::Detail("the case"), ANY, FLOW;
    FLOW_BRANCH_CONDITION_UNMATCHED: "flow.branch.condition", "unmatched", Reason::Detail("the case"), ANY, FLOW;

    SEND_GATE_PASSED: "send.gate", "passed", Reason::Detail("the gate"), ANY, SEND_GATE;
    SEND_GATE_BLOCKED: "send.gate", "blocked", Reason::Detail("the gate"), ANY, SEND_GATE;
    MESSAGE_SEND_SENT: "message.send", "sent", Reason::Detail("the send code"), ANY, MESSAGE;
    MESSAGE_SEND_SKIPPED: "message.send", "skipped", Reason::Detail("the gate that stopped it"), ANY, MESSAGE;
    MESSAGE_SEND_FAILED: "message.send", "failed", Reason::Detail("the failure code"), ANY, MESSAGE;
    MESSAGE_SEND_DROPPED: "message.send", "dropped", Reason::Detail("the gate that stopped it"), ANY, MESSAGE;
    MESSAGE_SEND_DEFERRED: "message.send", "deferred", Reason::Detail("the gate or limit that holds it"), ANY, MESSAGE;
    MESSAGE_SEND_NOTED: "message.send", "noted", Reason::Fixed("timezone_fallback"), ANY, MESSAGE;
    MESSAGE_FEEDBACK_DELIVERED: "message.feedback", "recorded", Reason::Fixed("delivered"), API, MESSAGE;
    MESSAGE_FEEDBACK_BOUNCED_HARD: "message.feedback", "recorded", Reason::Fixed("bounced_hard"), API, MESSAGE;
    MESSAGE_FEEDBACK_BOUNCED_SOFT: "message.feedback", "recorded", Reason::Fixed("bounced_soft"), API, MESSAGE;
    MESSAGE_FEEDBACK_COMPLAINED: "message.feedback", "recorded", Reason::Fixed("complained"), API, MESSAGE;
    SUPPRESSION_FEEDBACK_ORGANIZATION: "suppression.feedback", "added", Reason::Fixed("organization_hard_bounce"), API, MESSAGE;
    SUPPRESSION_FEEDBACK_WORKSPACE: "suppression.feedback", "added", Reason::Fixed("workspace_complaint"), API, MESSAGE;
    SUPPRESSION_FEEDBACK_UNABLE: "suppression.feedback", "unable", Reason::Fixed("address_hash_missing"), API, MESSAGE;

    WEBHOOK_CALL_SUCCEEDED: "webhook.call", "succeeded", Reason::Detail("the failure code, none on success"), ANY, CALL;
    WEBHOOK_CALL_FAILED: "webhook.call", "failed", Reason::Detail("the failure code"), ANY, CALL;
    SYNC_UPSERT_SUCCEEDED: "sync.upsert", "succeeded", Reason::Detail("the failure code, none on success"), ANY, CALL;
    SYNC_UPSERT_FAILED: "sync.upsert", "failed", Reason::Detail("the failure code"), ANY, CALL;
    SYNC_REMOVE_SUCCEEDED: "sync.remove", "succeeded", Reason::Detail("the failure code, none on success"), ANY, CALL;
    SYNC_REMOVE_FAILED: "sync.remove", "failed", Reason::Detail("the failure code"), ANY, CALL;
    EVENT_FORWARD_RETRY: "event.forward", "retry", Reason::Detail("the failure code"), ANY, FORWARD;
    EVENT_FORWARD_DEAD: "event.forward", "dead", Reason::Detail("the failure code"), ANY, FORWARD;

    SYNC_RECORD_APPLIED: "sync.record", "applied", Reason::None, SYNC, &["person", "source_record", "event"];
    SYNC_RECORD_DELETED: "sync.record", "deleted", Reason::Detail("why the record was deleted"), SYNC, &["source_record"];
    SYNC_RECORD_REJECTED: "sync.record", "rejected", Reason::Detail("the refusal code"), SYNC, &["source_record", "ingest_reject", "person"];
    SEGMENT_ENTERED: "segment.entered", "entered", Reason::Detail("the cause of the membership change"), ANY, &["segment", "person"];
    SEGMENT_EXITED: "segment.exited", "exited", Reason::Detail("the cause of the membership change"), ANY, &["segment", "person"];
    SEGMENT_REFRESH_SKIPPED: "segment.refresh", "skipped", Reason::Fixed("archived_or_gone"), ANY, &["segment"];
    SEGMENT_REFRESH_FAILED: "segment.refresh", "failed", Reason::Fixed("definition_invalid"), ANY, &["segment"];
    SEGMENT_REEVALUATE_FAILED: "segment.reevaluate", "failed", Reason::Fixed("definition_invalid"), ANY, &["segment"];

    JOB_ATTEMPT_FAILED: "job.attempt", "failed", Reason::Detail("the error code"), ANY, &[];
    JOB_ATTEMPT_EXHAUSTED: "job.attempt", "exhausted", Reason::Detail("the error code"), ANY, &[];
    JOB_ATTEMPT_LEASE_EXPIRED: "job.attempt", "lease_expired", Reason::Detail("the lease code"), ANY, &[];
    JOB_RETRY_REQUEUED: "job.retry", "requeued", Reason::None, &[CauseKind::Manual], &[];
}

/// The branch decision of a condition list: `case_N` for the first condition that matched, else
/// `case_more` past the tenth, else `else`.
pub const BRANCH_CASES: [&DecisionSpec; 10] = [
    &FLOW_BRANCH_CASE_0,
    &FLOW_BRANCH_CASE_1,
    &FLOW_BRANCH_CASE_2,
    &FLOW_BRANCH_CASE_3,
    &FLOW_BRANCH_CASE_4,
    &FLOW_BRANCH_CASE_5,
    &FLOW_BRANCH_CASE_6,
    &FLOW_BRANCH_CASE_7,
    &FLOW_BRANCH_CASE_8,
    &FLOW_BRANCH_CASE_9,
];

/// The distinct actions, in table order.
pub fn actions() -> Vec<&'static str> {
    let mut seen: Vec<&'static str> = Vec::new();
    for spec in ALL {
        if !seen.contains(&spec.action) {
            seen.push(spec.action);
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use hashbrown::HashSet;

    use super::*;

    fn snake_case(text: &str) -> bool {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            && !text.starts_with('_')
            && !text.ends_with('_')
    }

    #[test]
    fn every_row_has_a_unique_triple() {
        let mut seen = HashSet::new();
        for spec in ALL {
            let key = (spec.action, spec.outcome, spec.reason);
            let key = match key.2 {
                Reason::None => (key.0, key.1, "-none".to_owned()),
                Reason::Fixed(code) => (key.0, key.1, format!("fixed:{code}")),
                Reason::Detail(_) => (key.0, key.1, "-detail".to_owned()),
            };
            assert!(seen.insert(key), "{} repeats a triple", spec.action);
        }
        assert_eq!(ALL.len(), NAMES.len());
    }

    #[test]
    fn a_detail_row_is_the_only_row_of_its_action_and_outcome() {
        for spec in ALL.iter().filter(|s| matches!(s.reason, Reason::Detail(_))) {
            let others = ALL
                .iter()
                .filter(|other| other.action == spec.action && other.outcome == spec.outcome)
                .count();
            assert_eq!(
                others, 1,
                "{} {} mixes detail and fixed",
                spec.action, spec.outcome
            );
        }
    }

    #[test]
    fn every_row_names_causes_that_exist_and_subjects_in_snake_case() {
        for spec in ALL {
            assert!(!spec.causes.is_empty(), "{} has no cause", spec.action);
            for cause in spec.causes {
                assert_eq!(cause.as_str().parse::<CauseKind>().unwrap(), *cause);
            }
            for subject in spec.subjects {
                assert!(snake_case(subject), "{subject} is not snake_case");
            }
        }
    }

    #[test]
    fn actions_outcomes_and_fixed_reasons_are_snake_case() {
        for spec in ALL {
            for part in spec.action.split('.') {
                assert!(snake_case(part), "action {}", spec.action);
            }
            assert!(snake_case(spec.outcome), "outcome {}", spec.outcome);
            if let Reason::Fixed(code) = spec.reason {
                assert!(snake_case(code), "reason {code}");
            }
        }
    }

    #[test]
    fn trait_change_is_not_a_cause_of_any_row() {
        for spec in ALL {
            assert!(
                spec.causes
                    .iter()
                    .all(|cause| cause.as_str() != "trait_change")
            );
        }
    }

    #[test]
    fn reason_text_is_the_code_the_detail_or_nothing() {
        assert_eq!(FLOW_WAIT_DURATION.reason_text(Some("x")), Some("duration"));
        assert_eq!(FLOW_STEP_DONE.reason_text(Some("email")), Some("email"));
        assert_eq!(FLOW_STEP_DONE.reason_text(None), None);
        assert_eq!(FLOW_WAKE_TIMER.reason_text(Some("x")), None);
    }

    #[test]
    fn the_branch_cases_are_ten_rows_named_in_order() {
        for (index, spec) in BRANCH_CASES.iter().enumerate() {
            assert_eq!(spec.outcome, format!("case_{index}"));
        }
    }

    #[test]
    fn actions_lists_each_action_once() {
        let listed = actions();
        let unique: HashSet<_> = listed.iter().collect();
        assert_eq!(listed.len(), unique.len());
        assert!(listed.contains(&"flow.step"));
    }
}
