//! The no-personal-data invariant for decision-producing acceptance tests (architecture 12.4).

use std::future::Future;

use futures_util::TryStreamExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use zirv_flow_core::decision_actions::{self, Reason};

/// Distinctive values shared by the inputs and the invariant. Even the names are long enough
/// that a random UUID cannot accidentally contain one.
pub const PERSONAL_DATA: PersonalData = PersonalData {
    email: "ada@example.com",
    phone: "+45938373831",
    first_name: "Tracecanaryfirstname",
    last_name: "Tracecanarylastname",
    property: "trace_canary_property_383",
    skip_reason: "Please skip for Tracecanaryoperator's private reason",
    external_id: "trace-canary-external-383",
    segment_name: "Tracecanarysegment's private contacts",
    list_name: "Tracecanarylist's private contacts",
};

/// Inputs for the shared fixture and for domain-specific acceptance fixtures.
pub struct PersonalData {
    pub email: &'static str,
    pub phone: &'static str,
    pub first_name: &'static str,
    pub last_name: &'static str,
    pub property: &'static str,
    pub skip_reason: &'static str,
    pub external_id: &'static str,
    pub segment_name: &'static str,
    pub list_name: &'static str,
}

impl PersonalData {
    pub const fn markers(&self) -> [&'static str; 9] {
        [
            self.email,
            self.phone,
            self.first_name,
            self.last_name,
            self.property,
            self.skip_reason,
            self.external_id,
            self.segment_name,
            self.list_name,
        ]
    }

    pub fn identifiers(&self) -> Vec<(String, String)> {
        vec![
            ("email".into(), self.email.into()),
            ("phone".into(), self.phone.into()),
            ("external:trace_canary".into(), self.external_id.into()),
        ]
    }

    pub fn traits(&self) -> serde_json::Map<String, Value> {
        json!({"first_name": self.first_name, "last_name": self.last_name})
            .as_object()
            .unwrap()
            .clone()
    }

    pub fn properties(&self) -> serde_json::Map<String, Value> {
        json!({"private_note": self.property})
            .as_object()
            .unwrap()
            .clone()
    }
}

fn code(value: &str) -> bool {
    value.split('.').all(|part| {
        part.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
            && part
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    })
}

fn personal_data(value: &Value) -> Result<(), &'static str> {
    match value {
        Value::String(text) => {
            if text.contains('@') {
                return Err("cell contains @");
            }
            if PERSONAL_DATA
                .markers()
                .iter()
                .any(|marker| text.contains(marker))
            {
                return Err("cell contains a seeded marker");
            }
        }
        Value::Array(values) => {
            for value in values {
                personal_data(value)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                personal_data(&Value::String(key.clone()))?;
                personal_data(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Checks a trace response too, including values nested inside its tree.
pub fn assert_no_personal_data(value: &Value) {
    if let Err(reason) = personal_data(value) {
        panic!("{reason}");
    }
}

fn check_decision(row: &Value) -> Result<(), &'static str> {
    let subjects = row["subject_ids"]
        .as_object()
        .ok_or("subject_ids is not an object")?;
    for (key, value) in subjects {
        match key.as_str() {
            "node" => {
                if !value.as_str().is_some_and(zirv_flow_core::flow::valid_id) {
                    return Err("node is not a valid graph id");
                }
            }
            "version" => {
                if !value.is_number() {
                    return Err("version is not a number");
                }
            }
            _ => {
                if !decision_actions::ALL
                    .iter()
                    .any(|spec| spec.subjects().contains(&key.as_str()))
                {
                    return Err("unknown subject key");
                }
                if !value.as_str().is_some_and(|id| Uuid::parse_str(id).is_ok()) {
                    return Err("subject is not a UUID");
                }
            }
        }
    }
    for column in ["action", "outcome", "reason"] {
        if column == "reason" && row[column].is_null() {
            continue;
        }
        if !row[column].as_str().is_some_and(code) {
            return Err(match column {
                "action" => "action is not a code",
                "outcome" => "outcome is not a code",
                _ => "reason is not a code",
            });
        }
    }
    let reason = row["reason"].as_str();
    if !decision_actions::ALL.iter().any(|spec| {
        row["action"] == spec.action()
            && row["outcome"] == spec.outcome()
            && match spec.reason() {
                Reason::None => reason.is_none(),
                Reason::Fixed(expected) => reason == Some(expected),
                Reason::Detail(_) => true,
            }
    }) {
        return Err("triple is outside the decision registry");
    }
    let cause = row["cause_id"].as_str().ok_or("cause_id is not text")?;
    let kind = row["cause_kind"].as_str().ok_or("cause_kind is not text")?;
    let valid_cause = match kind {
        // The API's request_id::is_valid rule. The store cannot depend on the API crate.
        "api" => {
            (1..=128).contains(&cause.len())
                && cause
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        }
        "event" | "schedule" | "manual" | "definition_change" | "sync_run" => {
            Uuid::parse_str(cause).is_ok()
        }
        _ => false,
    };
    if !valid_cause {
        return Err("cause_id is not a UUID, request id or schedule id");
    }
    personal_data(row)
}

/// Reads the partitioned parent (including DEFAULT and every monthly partition), without a
/// workspace, trace, retention-window or page limit. Scan every cell, including future columns.
/// Jobs may legitimately carry personal data while live (e.g. destination erasure); dead jobs
/// must retain neither those payload values nor diagnostics containing them.
pub async fn assert_decisions_hold_no_personal_data(pool: &PgPool) {
    let mut decisions =
        sqlx::query_scalar::<_, Value>("SELECT to_jsonb(d) FROM decision_records d").fetch(pool);
    while let Some(row) = decisions.try_next().await.expect("read decision records") {
        if let Err(reason) = check_decision(&row) {
            panic!("decision {}: {reason}", row["id"]);
        }
    }
    let mut jobs = sqlx::query_as::<_, (Uuid, Value, Option<String>)>(
        "SELECT id, payload, last_error FROM jobs WHERE state = 'dead'",
    )
    .fetch(pool);
    while let Some((id, payload, error)) = jobs.try_next().await.expect("read dead jobs") {
        for (column, value) in [("payload", payload), ("last_error", json!(error))] {
            if let Err(reason) = personal_data(&value) {
                panic!("dead job {id} {column}: {reason}");
            }
        }
    }
}

/// Wrap a database acceptance test so even an early successful return checks every committed
/// decision. Keeping this at the fixture boundary makes the closing assertion unconditional.
pub async fn checked<F, Fut>(pool: PgPool, test: F)
where
    F: FnOnce(PgPool) -> Fut,
    Fut: Future<Output = ()>,
{
    let decision_pool = pool.clone();
    crate::migrate(&decision_pool)
        .await
        .expect("migrate decision fixture");
    test(pool).await;
    assert_decisions_hold_no_personal_data(&decision_pool).await;
}

/// Seeds every marker into its own data class in an existing workspace. The returned person
/// can be used by the suite's consent, segment, flow, delivery and erasure operations.
pub async fn seed_personal_data(pool: &PgPool, workspace: zirv_flow_core::WorkspaceId) -> Uuid {
    use time::OffsetDateTime;
    use zirv_flow_core::{Cause, HmacPurpose, HmacRoot, MasterKey, TraceContext, UserId};

    let trace = TraceContext::root(Cause::api("decision-fixture"));
    let profile = crate::ProfileRecord {
        source: "decision_fixture".into(),
        stream: "people".into(),
        record_key: PERSONAL_DATA.external_id.into(),
        identifiers: PERSONAL_DATA.identifiers(),
        traits: PERSONAL_DATA.traits(),
        observed_at: OffsetDateTime::now_utc(),
        trace: trace.clone(),
    };
    let outcomes = crate::apply_profile_batch(pool, workspace, &[profile])
        .await
        .unwrap();
    let person = match outcomes[0] {
        crate::ProfileOutcome::Applied { person_id, .. }
        | crate::ProfileOutcome::Unchanged { person_id, .. } => person_id,
        _ => panic!("the personal-data fixture was rejected"),
    };
    let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
    let event = crate::EventRecord {
        source: "decision_fixture".into(),
        source_record_id: None,
        identifiers: PERSONAL_DATA.identifiers(),
        name: "trace_canary".into(),
        properties: PERSONAL_DATA.properties(),
        timestamp: None,
        received_at: OffsetDateTime::now_utc(),
        message_id: Some("decision-fixture-event".into()),
        trace: trace.clone(),
    };
    assert!(matches!(
        crate::apply_event_batch(pool, workspace, &key, &[event])
            .await
            .unwrap()[0],
        crate::EventOutcome::Accepted { .. }
    ));
    let list = crate::create_list(pool, workspace, PERSONAL_DATA.list_name, "", None)
        .await
        .unwrap();
    let mut tx = crate::begin(pool).await.unwrap();
    crate::add_members(
        &mut tx,
        workspace,
        list.id,
        &[person],
        crate::ListSource::Api,
        None,
        &trace,
    )
    .await
    .unwrap();
    crate::create_segment(&mut tx, workspace, &crate::NewSegment {
        id: Uuid::now_v7(), name: PERSONAL_DATA.segment_name.into(), description: String::new(),
        cadence_minutes: crate::DEFAULT_CADENCE_MINUTES,
    }, &crate::NewSegmentVersion {
        definition: json!({"v": 1, "trait": "first_name", "op": "eq", "value": PERSONAL_DATA.first_name}),
        normalized: json!({"v": 1, "trait": "first_name", "op": "eq", "value": PERSONAL_DATA.first_name}),
        deps: json!({}), time_sensitive: false,
    }, None, &trace).await.unwrap();

    // An operator skips a failure whose diagnostic contains private free text. The skip must
    // record only "operator", even though the text is available on the locked enrollment.
    let flow = Uuid::now_v7();
    let enrollment = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO flows (id, workspace_id, name, draft_graph, draft_settings) \
                 VALUES ($1, $2, 'Decision fixture', '{}', '{}')",
    )
    .bind(flow)
    .bind(workspace.as_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO flow_versions (workspace_id, flow_id, version, graph, settings, trace_id) \
                 VALUES ($1, $2, 1, $3, '{}', $4)",
    )
    .bind(workspace.as_uuid())
    .bind(flow)
    .bind(json!({"v": 1, "nodes": [{"id": "end", "type": "exit"}], "edges": []}))
    .bind(trace.trace_id.as_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO enrollments (id, workspace_id, flow_id, flow_version, person_id, \
                    state, node, occurrence_id, error, failed_node, failure_placed, trace_id, cause_kind, cause_id) \
                 VALUES ($1, $2, $3, 1, $4, 'failed', 'end', $5, $6, 'end', true, $7, 'api', 'decision-fixture')")
        .bind(enrollment).bind(workspace.as_uuid()).bind(flow).bind(person).bind(Uuid::now_v7())
        .bind(PERSONAL_DATA.skip_reason).bind(trace.trace_id.as_uuid()).execute(&mut *tx).await.unwrap();
    crate::resume_failed(
        &mut tx,
        workspace,
        flow,
        enrollment,
        true,
        &TraceContext::root(Cause::manual(UserId::new())),
    )
    .await
    .unwrap();
    crate::commit(tx).await.unwrap();
    person
}

#[cfg(test)]
mod tests {
    use super::*;
    use zirv_flow_core::{Slug, WorkspaceId, WorkspaceName};

    async fn workspace(pool: &PgPool) -> WorkspaceId {
        crate::migrate(pool).await.unwrap();
        crate::create_workspace(
            pool,
            &WorkspaceName::parse("Decision privacy").unwrap(),
            &Slug::parse("decision-privacy").unwrap(),
        )
        .await
        .unwrap()
        .id
    }

    fn valid_row() -> Value {
        json!({"action": "flow.enroll", "outcome": "enrolled", "reason": null,
            "cause_kind": "api", "cause_id": "req-383",
            "subject_ids": {"person": Uuid::now_v7(), "node": "end", "version": 1}})
    }

    async fn insert(pool: &PgPool, ws: WorkspaceId, row: Value, old: bool) {
        sqlx::query("INSERT INTO decision_records (id, workspace_id, trace_id, recorded_at, \
                        action, outcome, reason, cause_kind, cause_id, subject_ids) \
                     VALUES ($1, $2, $3, CASE WHEN $4 THEN '2000-01-01'::timestamptz ELSE now() END, \
                             $5, $6, $7, $8, $9, $10)")
            .bind(Uuid::now_v7()).bind(ws.as_uuid()).bind(Uuid::now_v7()).bind(old)
            .bind(row["action"].as_str()).bind(row["outcome"].as_str()).bind(row["reason"].as_str())
            .bind(row["cause_kind"].as_str()).bind(row["cause_id"].as_str()).bind(&row["subject_ids"])
            .execute(pool).await.unwrap();
    }

    // Catch the public helper's panic, not a setup panic, and assert the particular diagnostic.
    async fn rejection(pool: &PgPool, expected: &str) {
        use futures_util::FutureExt;
        use std::panic::AssertUnwindSafe;
        let panic = AssertUnwindSafe(assert_decisions_hold_no_personal_data(pool))
            .catch_unwind()
            .await
            .expect_err("the invariant must reject the row");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap();
        assert!(message.contains(expected), "{message}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_email_in_a_uuid_subject_is_rejected(pool: PgPool) {
        let ws = workspace(&pool).await;
        let mut row = valid_row();
        row["subject_ids"]["person"] = json!(PERSONAL_DATA.email);
        insert(&pool, ws, row, false).await;
        rejection(&pool, "subject is not a UUID").await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_reason_with_a_space_is_rejected(pool: PgPool) {
        let ws = workspace(&pool).await;
        let mut row = valid_row();
        row["reason"] = json!("not a code");
        insert(&pool, ws, row, false).await;
        rejection(&pool, "reason is not a code").await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unknown_subject_is_rejected(pool: PgPool) {
        let ws = workspace(&pool).await;
        let mut row = valid_row();
        row["subject_ids"]["unregistered"] = json!(Uuid::now_v7());
        insert(&pool, ws, row, false).await;
        rejection(&pool, "unknown subject key").await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unregistered_action_is_rejected(pool: PgPool) {
        let ws = workspace(&pool).await;
        let mut row = valid_row();
        row["action"] = json!("unregistered.action");
        insert(&pool, ws, row, false).await;
        rejection(&pool, "triple is outside the decision registry").await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_trait_value_used_as_a_reason_is_rejected(pool: PgPool) {
        let ws = workspace(&pool).await;
        seed_personal_data(&pool, ws).await;
        let mut row = valid_row();
        row["action"] = json!("flow.enrollment");
        row["outcome"] = json!("failed");
        row["reason"] = json!(PERSONAL_DATA.first_name);
        insert(&pool, ws, row, false).await;
        rejection(&pool, "reason is not a code").await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn code_shaped_markers_in_the_default_partition_are_rejected(pool: PgPool) {
        let ws = workspace(&pool).await;
        let mut row = valid_row();
        row["action"] = json!("flow.enrollment");
        row["outcome"] = json!("failed");
        row["reason"] = json!(PERSONAL_DATA.property);
        insert(&pool, ws, row, true).await;
        rejection(&pool, "cell contains a seeded marker").await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn seeds_use_exactly_the_markers_the_scan_checks(pool: PgPool) {
        let ws = workspace(&pool).await;
        let person = seed_personal_data(&pool, ws).await;
        let mut actual: Vec<String> = sqlx::query_scalar(
            "SELECT value FROM identities WHERE person_id = $1 \
             UNION ALL SELECT traits ->> 'first_name' FROM people WHERE id = $1 \
             UNION ALL SELECT traits ->> 'last_name' FROM people WHERE id = $1 \
             UNION ALL SELECT properties ->> 'private_note' FROM events WHERE person_id = $1 \
             UNION ALL SELECT error FROM enrollments WHERE person_id = $1 \
             UNION ALL SELECT name FROM segments WHERE workspace_id = $2 \
             UNION ALL SELECT name FROM lists WHERE workspace_id = $2",
        )
        .bind(person)
        .bind(ws.as_uuid())
        .fetch_all(&pool)
        .await
        .unwrap();
        actual.sort();
        let mut expected = PERSONAL_DATA.markers().map(str::to_owned).to_vec();
        expected.sort();
        assert_eq!(actual, expected);
        insert(&pool, ws, valid_row(), false).await;
        insert(&pool, ws, valid_row(), true).await;
        let partitions: i64 =
            sqlx::query_scalar("SELECT count(DISTINCT tableoid) FROM decision_records")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(partitions, 2);
        assert_decisions_hold_no_personal_data(&pool).await;
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn dead_job_payloads_and_errors_are_checked(pool: PgPool) {
        use std::time::Duration;
        use zirv_flow_core::{Cause, TraceContext};
        let ws = workspace(&pool).await;
        crate::enqueue(
            &pool,
            &crate::NewJob {
                workspace_id: Some(ws),
                kind: "test".into(),
                payload: json!({}),
                priority: 0,
                unique_key: None,
                run_at: None,
                max_attempts: 1,
                trace: TraceContext::root(Cause::api("req-383")),
            },
        )
        .await
        .unwrap();
        let job = crate::claim_job(&pool, "privacy", &["test"], Duration::from_secs(30), None)
            .await
            .unwrap()
            .unwrap();
        let id = job.id;
        crate::fail_job(&pool, id, job.lease_token, Duration::ZERO, "test_failed")
            .await
            .unwrap();
        for column in ["payload", "last_error"] {
            sqlx::query("UPDATE jobs SET payload = $2, last_error = $3 WHERE id = $1")
                .bind(id)
                .bind(if column == "payload" {
                    json!({"nested": [PERSONAL_DATA.property]})
                } else {
                    json!({})
                })
                .bind(if column == "last_error" {
                    PERSONAL_DATA.last_name
                } else {
                    "test_failed"
                })
                .execute(&pool)
                .await
                .unwrap();
            rejection(&pool, &format!("{column}: cell contains a seeded marker")).await;
        }
    }

    #[test]
    fn malformed_graph_points_causes_and_triples_are_rejected() {
        for (key, value, expected) in [
            (
                "node",
                json!("private node"),
                "node is not a valid graph id",
            ),
            (
                "node",
                json!("x".repeat(65)),
                "node is not a valid graph id",
            ),
            ("version", json!("1"), "version is not a number"),
        ] {
            let mut row = valid_row();
            row["subject_ids"][key] = value;
            assert_eq!(check_decision(&row), Err(expected));
        }
        for (key, value, expected) in [
            (
                "cause_id",
                "free text",
                "cause_id is not a UUID, request id or schedule id",
            ),
            (
                "outcome",
                "unregistered",
                "triple is outside the decision registry",
            ),
            (
                "reason",
                "unregistered",
                "triple is outside the decision registry",
            ),
        ] {
            let mut row = valid_row();
            row[key] = json!(value);
            assert_eq!(check_decision(&row), Err(expected));
        }
        for kind in [
            "event",
            "schedule",
            "manual",
            "definition_change",
            "sync_run",
        ] {
            let mut row = valid_row();
            row["cause_kind"] = json!(kind);
            assert!(check_decision(&row).is_err());
            row["cause_id"] = json!(Uuid::now_v7());
            assert_eq!(check_decision(&row), Ok(()));
        }
        assert_eq!(
            personal_data(&json!({"nested": ["other@example.com"]})),
            Err("cell contains @")
        );
    }
}
