//! Test support: the no-personal-data invariant over every decision record (architecture section
//! 12.4, "never stores identifier, trait or event-property values"), and the personal values the
//! acceptance suites seed to prove it.
//!
//! A suite that writes decisions seeds [`seed_personal_values`] into its workspace and ends with
//! [`assert_decisions_hold_no_personal_data`]. The helper reads every stored decision, partitions
//! included, and the `payload` and `last_error` of every job.

use std::fmt::Write as _;

use hashbrown::HashSet;
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;
use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus};
use zirv_flow_core::decision_actions::{self, DecisionSpec, Reason};
use zirv_flow_core::flow::valid_id;
use zirv_flow_core::{
    Cause, CauseKind, HmacKey, HmacPurpose, HmacRoot, MasterKey, TraceContext, WorkspaceId,
};

use crate::{
    ConsentChange, EventRecord, ListSource, NewSegment, NewSegmentVersion, ProfileRecord,
    add_members, apply_event_batch, apply_profile_batch, begin, commit, create_list,
    create_segment, set_consent,
};

/// The email of the seeded person.
pub const EMAIL: &str = "zq.seed.ada@zqmarker.example";
/// The phone number of the seeded person.
pub const PHONE: &str = "+4570125634";
/// The first-name trait of the seeded person.
pub const FIRST_NAME: &str = "Zqadaline";
/// The last-name trait of the seeded person.
pub const LAST_NAME: &str = "Zqlovelacey";
/// A property value on the seeded person's event.
pub const PROPERTY_VALUE: &str = "zq-property-8841";
/// The free-text note an operator might type with a skip or a consent change.
pub const OPERATOR_NOTE: &str = "zq operator note: she asked us to stop";
/// An id from an outside system.
pub const EXTERNAL_ID: &str = "zq-ext-5521";
/// A segment name that looks like personal data.
pub const SEGMENT_NAME: &str = "Zq Ada Lovelace big spenders";
/// A list name that looks like personal data.
pub const LIST_NAME: &str = "Zq Grace Hopper friends";

/// Every value the seeds plant and no decision, job payload or job error may hold. The seeds use
/// these constants and nothing else, so the helper and the seeds cannot drift.
pub const PERSONAL_MARKERS: [&str; 9] = [
    EMAIL,
    PHONE,
    FIRST_NAME,
    LAST_NAME,
    PROPERTY_VALUE,
    OPERATOR_NOTE,
    EXTERNAL_ID,
    SEGMENT_NAME,
    LIST_NAME,
];

/// The longest request id a cause carries.
const MAX_REQUEST_ID: usize = 64;

/// What [`seed_personal_values`] planted.
#[derive(Clone, Debug)]
pub struct PersonalSeed {
    pub person: Uuid,
    /// The seeded values, as the seeds wrote them.
    pub written: Vec<&'static str>,
}

fn trace() -> TraceContext {
    TraceContext::root(Cause::api("seed"))
}

fn redaction_key() -> HmacKey {
    let master = MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=")
        .expect("the fixed master key parses");
    HmacRoot::bootstrap_from(&master).hmac_key(HmacPurpose::Redaction)
}

/// Plants every personal value of [`PERSONAL_MARKERS`] in `workspace`: a person holding the email,
/// the phone number, the external id and the two name traits; an event with the property value;
/// consent whose evidence carries the operator note; a list and a segment named like people. The
/// writes leave decisions behind, which the invariant then checks.
pub async fn seed_personal_values(pool: &PgPool, workspace: WorkspaceId) -> PersonalSeed {
    let mut written = Vec::new();
    let outcomes = apply_profile_batch(
        pool,
        workspace,
        &[ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: "zq-1".into(),
            identifiers: vec![
                ("email".into(), EMAIL.into()),
                ("phone".into(), PHONE.into()),
                ("external:crm".into(), EXTERNAL_ID.into()),
            ],
            traits: json!({"first_name": FIRST_NAME, "last_name": LAST_NAME})
                .as_object()
                .cloned()
                .expect("an object"),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: trace(),
        }],
    )
    .await
    .expect("the seeded person is written");
    let person = match outcomes.first() {
        Some(crate::ProfileOutcome::Applied { person_id, .. })
        | Some(crate::ProfileOutcome::Unchanged { person_id, .. }) => *person_id,
        other => panic!("the seeded person was not applied: {other:?}"),
    };
    written.extend([EMAIL, PHONE, EXTERNAL_ID, FIRST_NAME, LAST_NAME]);

    apply_event_batch(
        pool,
        workspace,
        &redaction_key(),
        &[EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), EMAIL.into())],
            name: "seeded".into(),
            properties: json!({"plan": PROPERTY_VALUE})
                .as_object()
                .cloned()
                .expect("an object"),
            timestamp: None,
            received_at: OffsetDateTime::now_utc(),
            message_id: Some("zq-m-1".into()),
            trace: trace(),
        }],
    )
    .await
    .expect("the seeded event is written");
    written.push(PROPERTY_VALUE);

    let mut tx = begin(pool).await.expect("a transaction");
    set_consent(
        &mut tx,
        workspace,
        &ConsentChange {
            dimensions: Default::default(),
            person,
            channel: Channel::Email,
            topic: None,
            status: ConsentStatus::Subscribed,
            source: ChangeSource::Api,
            evidence: json!({"note": OPERATOR_NOTE}),
            occurred_at: datetime!(2026-01-02 00:00 UTC),
            actor: None,
        },
        &trace(),
    )
    .await
    .expect("the seeded consent is written");
    commit(tx).await.expect("the commit");
    written.push(OPERATOR_NOTE);

    let list = create_list(pool, workspace, LIST_NAME, "", None)
        .await
        .expect("the seeded list is created");
    let mut tx = begin(pool).await.expect("a transaction");
    add_members(
        &mut tx,
        workspace,
        list.id,
        &[person],
        ListSource::Api,
        None,
        &trace(),
    )
    .await
    .expect("the seeded member is added");
    commit(tx).await.expect("the commit");
    written.push(LIST_NAME);

    let mut tx = begin(pool).await.expect("a transaction");
    create_segment(
        &mut tx,
        workspace,
        &NewSegment {
            id: Uuid::now_v7(),
            name: SEGMENT_NAME.into(),
            description: String::new(),
            cadence_minutes: 60,
        },
        &NewSegmentVersion {
            definition: json!({"v": 1}),
            normalized: json!({"v": 1}),
            deps: json!({"segments": []}),
            time_sensitive: false,
        },
        None,
        &trace(),
    )
    .await
    .expect("the seeded segment is created");
    commit(tx).await.expect("the commit");
    written.push(SEGMENT_NAME);

    PersonalSeed { person, written }
}

/// One stored decision, as the invariant reads it.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct StoredDecision {
    pub action: String,
    pub outcome: String,
    pub reason: Option<String>,
    pub cause_kind: String,
    pub cause_id: String,
    pub subject_ids: Value,
    /// Every cell of the row as text, for the scans over the whole row.
    pub text: String,
}

/// Whether `text` is a code: snake_case parts joined by dots, no spaces.
fn code_shaped(text: &str) -> bool {
    !text.is_empty()
        && text.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// The marker `text` holds, ignoring case, if any.
fn marker_in(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    PERSONAL_MARKERS
        .into_iter()
        .find(|marker| lower.contains(&marker.to_lowercase()))
}

fn request_id_shaped(text: &str) -> bool {
    (1..=MAX_REQUEST_ID).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn specs_of<'a>(
    action: &'a str,
    outcome: &'a str,
) -> impl Iterator<Item = &'static DecisionSpec> + 'a {
    decision_actions::ALL
        .iter()
        .copied()
        .filter(move |spec| spec.action() == action && spec.outcome() == outcome)
}

/// The ways `row` breaks the invariant, each naming the row's action and outcome and the cell,
/// never the value.
pub fn decision_violations(row: &StoredDecision) -> Vec<String> {
    let mut found = Vec::new();
    let at = format!("{} {}", row.action, row.outcome);

    let specs: Vec<&DecisionSpec> = specs_of(&row.action, &row.outcome).collect();
    if specs.is_empty() {
        found.push(format!(
            "{at}: the action and outcome are not in the registry"
        ));
    }
    if !code_shaped(&row.action) || !code_shaped(&row.outcome) {
        found.push(format!(
            "{at}: the action or the outcome is not code-shaped"
        ));
    }
    match row.reason.as_deref() {
        Some(reason) if !code_shaped(reason) => {
            found.push(format!("{at}: the reason is not code-shaped"));
        }
        reason => {
            let registered = specs.iter().any(|spec| match spec.reason() {
                Reason::None => reason.is_none(),
                Reason::Fixed(code) => reason == Some(code),
                Reason::Detail(_) => true,
            });
            if !registered && !specs.is_empty() {
                found.push(format!(
                    "{at}: the reason is not in the registry for this action"
                ));
            }
        }
    }

    let subject_names: HashSet<&str> = decision_actions::ALL
        .iter()
        .flat_map(|spec| spec.subjects().iter().copied())
        .collect();
    match row.subject_ids.as_object() {
        None => found.push(format!("{at}: subject_ids is not an object")),
        Some(subjects) => {
            for (key, value) in subjects {
                match key.as_str() {
                    "node" => {
                        if !value.as_str().is_some_and(valid_id) {
                            found.push(format!("{at}: the node is not a node id"));
                        }
                    }
                    "version" => {
                        if !value.as_i64().is_some_and(|number| number > 0) {
                            found.push(format!("{at}: the version is not a number"));
                        }
                    }
                    key if subject_names.contains(key) => {
                        if !value
                            .as_str()
                            .is_some_and(|text| text.parse::<Uuid>().is_ok())
                        {
                            found.push(format!("{at}: subject {key} is not a uuid"));
                        }
                    }
                    key => found.push(format!("{at}: subject {key} is not a registry subject")),
                }
            }
        }
    }

    match row.cause_kind.parse::<CauseKind>() {
        Err(_) => found.push(format!("{at}: the cause kind is unknown")),
        Ok(kind) => {
            let uuid = row.cause_id.parse::<Uuid>().is_ok();
            if !(uuid || (kind == CauseKind::Api && request_id_shaped(&row.cause_id))) {
                found.push(format!(
                    "{at}: the cause id is neither a uuid nor a request id"
                ));
            }
        }
    }

    if row.text.contains('@') {
        found.push(format!("{at}: a cell holds an @"));
    }
    if marker_in(&row.text).is_some() {
        found.push(format!("{at}: a cell holds a seeded personal value"));
    }
    found
}

/// The ways one job breaks the invariant: a seeded value in the payload or in the error, or an
/// `@` in the error or in the payload of a dead job.
pub fn job_violations(
    kind: &str,
    state: &str,
    payload: &str,
    last_error: Option<&str>,
) -> Vec<String> {
    let mut found = Vec::new();
    let at = format!("job {kind} ({state})");
    if marker_in(payload).is_some() {
        found.push(format!("{at}: the payload holds a seeded personal value"));
    }
    if state == "dead" && payload.contains('@') {
        found.push(format!("{at}: the payload holds an @"));
    }
    if let Some(error) = last_error {
        if marker_in(error).is_some() {
            found.push(format!("{at}: last_error holds a seeded personal value"));
        }
        if error.contains('@') {
            found.push(format!("{at}: last_error holds an @"));
        }
    }
    found
}

/// Every violation in the decision log and the job table of the database.
pub async fn decision_leaks(pool: &PgPool) -> Vec<String> {
    let rows: Vec<StoredDecision> = sqlx::query_as(
        "SELECT action, outcome, reason, cause_kind, cause_id, subject_ids, d::text AS text FROM decision_records d",
    )
    .fetch_all(pool)
    .await
    .expect("the decision records are read");
    let mut found = Vec::new();
    for row in &rows {
        found.extend(decision_violations(row));
    }
    let jobs: Vec<(String, String, String, Option<String>)> =
        sqlx::query_as("SELECT kind, state, payload::text, last_error FROM jobs")
            .fetch_all(pool)
            .await
            .expect("the jobs are read");
    for (kind, state, payload, last_error) in jobs {
        found.extend(job_violations(
            &kind,
            &state,
            &payload,
            last_error.as_deref(),
        ));
    }
    found.sort();
    found.dedup();
    found
}

/// Panics, naming each violation, unless every decision record, and the payload and error of every
/// job, are free of personal data: the subject ids are registry names holding ids, the action,
/// outcome and reason are codes in the registry, the cause is an id, and no cell holds an `@` or a
/// seeded value.
pub async fn assert_decisions_hold_no_personal_data(pool: &PgPool) {
    let found = decision_leaks(pool).await;
    if !found.is_empty() {
        let mut report = String::from("the decision log holds personal data or off-registry rows:");
        for line in &found {
            let _ = write!(report, "\n  {line}");
        }
        panic!("{report}");
    }
}

#[cfg(test)]
mod tests {
    use zirv_flow_core::decision_actions::{FLOW_STEP_DONE, Reason};
    use zirv_flow_core::{Cause, CauseKind, Slug, TraceContext, WorkspaceName};

    use super::*;
    use crate::{GraphPoint, NewDecision, append_decision, create_workspace};

    fn good() -> StoredDecision {
        let id = Uuid::now_v7().to_string();
        StoredDecision {
            action: "flow.step".into(),
            outcome: "done".into(),
            reason: Some("email".into()),
            cause_kind: "api".into(),
            cause_id: "req-1".into(),
            subject_ids: json!({"flow": id, "person": id, "node": "welcome", "version": 3}),
            text: "flow.step done".into(),
        }
    }

    async fn workspace(pool: &PgPool) -> WorkspaceId {
        create_workspace(
            pool,
            &WorkspaceName::parse("one").unwrap(),
            &Slug::parse("one").unwrap(),
        )
        .await
        .unwrap()
        .id
    }

    #[test]
    fn a_clean_row_has_no_violation() {
        assert_eq!(decision_violations(&good()), Vec::<String>::new());
    }

    #[test]
    fn a_subject_that_is_an_email_is_rejected() {
        let mut row = good();
        row.subject_ids = json!({"person": EMAIL});
        row.text = format!("{{\"person\": \"{EMAIL}\"}}");

        let found = decision_violations(&row);

        assert!(
            found
                .iter()
                .any(|line| line.contains("subject person is not a uuid"))
        );
        assert!(found.iter().any(|line| line.contains("holds an @")));
    }

    #[test]
    fn a_reason_with_a_space_is_rejected() {
        let mut row = good();
        row.reason = Some("lease expired".into());

        let found = decision_violations(&row);

        assert!(
            found
                .iter()
                .any(|line| line.contains("reason is not code-shaped"))
        );
    }

    #[test]
    fn an_unknown_subject_key_is_rejected() {
        let mut row = good();
        row.subject_ids = json!({"email_address": Uuid::now_v7().to_string()});

        let found = decision_violations(&row);

        assert!(
            found
                .iter()
                .any(|line| line.contains("not a registry subject"))
        );
    }

    #[test]
    fn an_action_outside_the_registry_is_rejected() {
        let mut row = good();
        row.action = "flow.teleport".into();

        let found = decision_violations(&row);

        assert!(
            found
                .iter()
                .any(|line| line.contains("not in the registry"))
        );
    }

    #[test]
    fn a_fixed_reason_must_match_the_registry() {
        let mut row = good();
        row.action = "flow.wait".into();
        row.outcome = "waiting".into();
        row.reason = Some("whenever".into());

        let found = decision_violations(&row);

        assert!(
            found
                .iter()
                .any(|line| line.contains("reason is not in the registry"))
        );
    }

    #[test]
    fn a_node_that_is_not_a_node_id_and_a_version_that_is_not_a_number_are_rejected() {
        let mut row = good();
        row.subject_ids = json!({"node": "ada@example.com", "version": "three"});

        let found = decision_violations(&row);

        assert!(found.iter().any(|line| line.contains("not a node id")));
        assert!(
            found
                .iter()
                .any(|line| line.contains("version is not a number"))
        );
    }

    #[test]
    fn a_cause_that_is_free_text_is_rejected() {
        let mut row = good();
        row.cause_id = "ada called us".into();
        assert!(
            decision_violations(&row)
                .iter()
                .any(|line| line.contains("cause id"))
        );
        row.cause_id = "req-1".into();
        row.cause_kind = "schedule".into();
        assert!(
            decision_violations(&row)
                .iter()
                .any(|line| line.contains("cause id"))
        );
    }

    #[test]
    fn every_marker_is_caught_in_a_cell() {
        for marker in PERSONAL_MARKERS {
            let mut row = good();
            row.text = format!("flow.step done {}", marker.to_uppercase());
            assert!(
                decision_violations(&row)
                    .iter()
                    .any(|line| line.contains("seeded personal value")),
                "{marker}"
            );
        }
    }

    #[test]
    fn a_job_payload_or_error_with_a_marker_or_an_address_is_rejected() {
        assert!(job_violations("k", "ready", "{}", None).is_empty());
        assert!(
            !job_violations("k", "ready", &format!("{{\"to\":\"{EXTERNAL_ID}\"}}"), None)
                .is_empty()
        );
        assert!(!job_violations("k", "dead", "{\"to\":\"a@b.c\"}", None).is_empty());
        assert!(job_violations("k", "ready", "{\"to\":\"a@b.c\"}", None).is_empty());
        assert!(!job_violations("k", "dead", "{}", Some("rejected a@b.c")).is_empty());
        assert!(!job_violations("k", "dead", "{}", Some(LAST_NAME)).is_empty());
    }

    #[test]
    fn the_markers_are_distinct_and_the_seeds_are_what_they_list() {
        let unique: HashSet<_> = PERSONAL_MARKERS.iter().collect();
        assert_eq!(unique.len(), PERSONAL_MARKERS.len());
        assert!(EMAIL.contains('@'));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_seeds_write_exactly_the_marker_list_and_the_data_holds_them(pool: PgPool) {
        let ws = workspace(&pool).await;

        let seed = seed_personal_values(&pool, ws).await;

        let written: HashSet<&str> = seed.written.iter().copied().collect();
        let listed: HashSet<&str> = PERSONAL_MARKERS.into_iter().collect();
        assert_eq!(written, listed);
        for (table, marker) in [
            ("people", FIRST_NAME),
            ("people", LAST_NAME),
            ("lists", LIST_NAME),
            ("segments", SEGMENT_NAME),
            ("events", PROPERTY_VALUE),
            ("consent_changes", OPERATOR_NOTE),
        ] {
            let held: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT string_agg(t::text, ' ') FROM {table} t"
            )))
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(
                held.unwrap_or_default().contains(marker),
                "{table} does not hold its seeded value"
            );
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_invariant_holds_over_the_decisions_the_seeds_leave(pool: PgPool) {
        let ws = workspace(&pool).await;
        seed_personal_values(&pool, ws).await;
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM decision_records")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(count > 0, "the seeds left no decision to check");

        assert_decisions_hold_no_personal_data(&pool).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_decision_with_a_trait_value_as_its_reason_is_caught(pool: PgPool) {
        let ws = workspace(&pool).await;
        let leaked: &'static str = Box::leak(FIRST_NAME.to_owned().into_boxed_str());
        static LEAKY: DecisionSpec = DecisionSpec::for_test(
            "flow.step",
            "done",
            Reason::Detail("a trait value"),
            &[CauseKind::Api],
            &["flow"],
        );
        append_decision(
            &pool,
            &NewDecision {
                workspace_id: ws,
                trace: TraceContext::root(Cause::api("leak")),
                spec: &LEAKY,
                detail: Some(leaked),
                subject_ids: vec![("flow", Uuid::now_v7())],
                job: None,
                graph: Some(GraphPoint::at(1, "welcome")),
            },
        )
        .await
        .unwrap();

        let found = decision_leaks(&pool).await;

        assert!(
            found
                .iter()
                .any(|line| line.contains("seeded personal value")),
            "{found:?}"
        );
        assert!(
            found.iter().any(|line| line.contains("not code-shaped")) || found.len() == 1,
            "{found:?}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_dead_job_error_that_echoes_a_value_is_caught(pool: PgPool) {
        sqlx::query(
            "INSERT INTO jobs (id, kind, payload, state, max_attempts, last_error, trace_id, \
                cause_kind, cause_id) \
             VALUES (gen_random_uuid(), 'k', '{}', 'dead', 1, $1, gen_random_uuid(), 'api', 'x')",
        )
        .bind(format!("the server refused {EMAIL}"))
        .execute(&pool)
        .await
        .unwrap();

        let found = decision_leaks(&pool).await;

        assert!(
            found.iter().any(|line| line.contains("last_error")),
            "{found:?}"
        );
    }

    #[test]
    fn the_step_row_the_leak_test_imitates_is_registered() {
        assert!(matches!(FLOW_STEP_DONE.reason(), Reason::Detail(_)));
    }
}
