//! Test support: a workspace holding a person with every M2 data class.

use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceName;
use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus};
use zirv_flow_core::{
    Cause, HmacKey, HmacPurpose, HmacRoot, MasterKey, Slug, TraceContext, WorkspaceId,
};

use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_batch};
use crate::{
    ConsentChange, EventOutcome, EventPolicy, EventRecord, IngestItem, IngestPayload, ListSource,
    add_members, append_inbox, apply_event_batch, begin, commit, create_list, create_workspace,
    set_consent, set_event_policy,
};
use zirv_flow_core::events::UnplannedPolicy;

/// The email of the seeded person.
pub(crate) const EMAIL: &str = "ada@example.com";

/// The email of the person the seeded person shares a list with.
pub(crate) const BYSTANDER_EMAIL: &str = "grace@example.com";

/// What [`seed_everything`] built.
pub(crate) struct Seeded {
    pub(crate) ws: WorkspaceId,
    /// The survivor, with two source records, events, consent, lists and a conflict.
    pub(crate) person: Uuid,
    /// The person merged into `person`.
    pub(crate) merged: Uuid,
    /// An unrelated person who shares a list with `person` and a conflict with them.
    pub(crate) bystander: Uuid,
}

pub(crate) fn trace() -> TraceContext {
    TraceContext::root(Cause::api("t"))
}

pub(crate) fn master_key() -> MasterKey {
    MasterKey::parse("BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=").unwrap()
}

/// The key that hashes identifiers, as the API and the CLI derive it.
pub(crate) fn identifier_key() -> HmacKey {
    HmacRoot::bootstrap_from(&master_key()).hmac_key(HmacPurpose::Identifier)
}

pub(crate) fn profile(
    source: &str,
    key: &str,
    identifiers: &[(&str, &str)],
    traits: serde_json::Value,
) -> ProfileRecord {
    ProfileRecord {
        source: source.into(),
        stream: "contacts".into(),
        record_key: key.into(),
        identifiers: identifiers
            .iter()
            .map(|(kind, value)| ((*kind).into(), (*value).into()))
            .collect(),
        traits: traits.as_object().cloned().unwrap(),
        observed_at: datetime!(2026-01-01 00:00 UTC),
        trace: trace(),
    }
}

pub(crate) fn person_of(outcome: &ProfileOutcome) -> Uuid {
    match outcome {
        ProfileOutcome::Applied { person_id, .. } | ProfileOutcome::Unchanged { person_id, .. } => {
            *person_id
        }
        ProfileOutcome::Rejected { reason } => panic!("rejected: {reason}"),
    }
}

pub(crate) async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
    let name = WorkspaceName::parse(slug).unwrap();
    create_workspace(pool, &name, &Slug::parse(slug).unwrap())
        .await
        .unwrap()
        .id
}

fn event(name: &str, message_id: &str, at: Option<OffsetDateTime>) -> EventRecord {
    EventRecord {
        source_record_id: None,
        source: "web".into(),
        identifiers: vec![("email".into(), EMAIL.into())],
        name: name.into(),
        properties: json!({"plan": "pro"}).as_object().cloned().unwrap(),
        timestamp: at,
        received_at: OffsetDateTime::now_utc(),
        message_id: Some(message_id.into()),
        trace: trace(),
    }
}

/// Builds, in a fresh workspace:
/// - a person from two sources, with a third record that merged another person into them;
/// - two events in two monthly partitions, their dedupe rows and one ingest reject;
/// - consent with history, two lists with a bystander in one of them, and an identity
///   conflict with the bystander;
/// - flow enrollments with their steps, waits, entries and messages, saved preview people,
///   forwarding rows and a handled marker;
/// - a pending export;
/// - pending inbox rows for the person and for the bystander.
pub(crate) async fn seed_everything(pool: &PgPool) -> Seeded {
    let ws = workspace(pool, "one").await;
    let outcomes = apply_profile_batch(
        pool,
        ws,
        &[
            profile(
                "crm",
                "1",
                &[("external:crm", "1"), ("email", EMAIL)],
                json!({"plan": "free", "name": "Ada"}),
            ),
            profile(
                "shop",
                "9",
                &[("anonymous_id", "anon-1")],
                json!({"city": "Oslo"}),
            ),
            profile(
                "shop",
                "10",
                &[("email", EMAIL), ("anonymous_id", "anon-1")],
                json!({"plan": "pro"}),
            ),
            profile("crm", "2", &[("email", BYSTANDER_EMAIL)], json!({})),
        ],
    )
    .await
    .unwrap();
    let person = person_of(&outcomes[2]);
    let merged = [person_of(&outcomes[0]), person_of(&outcomes[1])]
        .into_iter()
        .find(|id| *id != person)
        .expect("the third record merged one of the first two people into the survivor");
    let bystander = person_of(&outcomes[3]);

    // Events in two months, one with a message id each, then a reject. The debugger watches,
    // so each outcome also leaves a feed row.
    crate::touch_debugger_watch(pool, ws).await.unwrap();
    let key = HmacRoot::bootstrap_from(&master_key()).hmac_key(HmacPurpose::Redaction);
    let now = OffsetDateTime::now_utc();
    let accepted = apply_event_batch(
        pool,
        ws,
        &key,
        &[
            event("seen", "m-1", Some(now - time::Duration::days(70))),
            event("seen", "m-2", Some(now - time::Duration::days(10))),
        ],
    )
    .await
    .unwrap();
    assert!(
        accepted
            .iter()
            .all(|outcome| matches!(outcome, EventOutcome::Accepted { .. })),
        "{accepted:?}"
    );
    // One event came from the person's CRM record, as a synced event will: removing or erasing
    // the record's person must take it along.
    sqlx::query(
        "UPDATE events SET source_record_id = \
            (SELECT id FROM source_records \
             WHERE workspace_id = $1 AND source = 'crm' AND record_key = '1') \
         WHERE workspace_id = $1 AND message_id = 'm-2'",
    )
    .bind(ws.as_uuid())
    .execute(pool)
    .await
    .unwrap();
    set_event_policy(
        pool,
        ws,
        &EventPolicy {
            unplanned: UnplannedPolicy::Reject,
            ..EventPolicy::default()
        },
    )
    .await
    .unwrap();
    apply_event_batch(pool, ws, &key, &[event("unknown", "m-3", None)])
        .await
        .unwrap();

    // Consent with a history of two changes.
    let mut tx = begin(pool).await.unwrap();
    for (status, at) in [
        (ConsentStatus::Subscribed, datetime!(2026-01-01 00:00 UTC)),
        (ConsentStatus::Unsubscribed, datetime!(2026-02-01 00:00 UTC)),
    ] {
        set_consent(
            &mut tx,
            ws,
            &ConsentChange {
                dimensions: Default::default(),
                person,
                channel: Channel::Email,
                topic: None,
                status,
                source: ChangeSource::Api,
                evidence: json!({"form": "signup"}),
                occurred_at: at,
                actor: None,
            },
            &trace(),
        )
        .await
        .unwrap();
    }
    commit(tx).await.unwrap();

    // Two lists; the bystander is in the first.
    let vips = create_list(pool, ws, "VIPs", "", None).await.unwrap();
    let news = create_list(pool, ws, "News", "", None).await.unwrap();
    let mut tx = begin(pool).await.unwrap();
    add_members(
        &mut tx,
        ws,
        vips.id,
        &[person, bystander],
        ListSource::Api,
        None,
        &trace(),
    )
    .await
    .unwrap();
    add_members(
        &mut tx,
        ws,
        news.id,
        &[person],
        ListSource::Api,
        None,
        &trace(),
    )
    .await
    .unwrap();
    commit(tx).await.unwrap();

    // A flow the person is enrolled in, asleep at an event wait, with its entry.
    let flow = Uuid::now_v7();
    let enrollment = Uuid::now_v7();
    let mut tx = begin(pool).await.unwrap();
    for statement in [
        "INSERT INTO flows (id, workspace_id, name, state, draft_graph, draft_settings, \
            current_version) VALUES ($1, $2, 'Seeded', 'published', '{}', '{}', 1)",
        "INSERT INTO flow_versions (workspace_id, flow_id, version, graph, settings, trace_id) \
         VALUES ($2, $1, 1, '{}', '{}', gen_random_uuid())",
        "INSERT INTO enrollments (id, workspace_id, flow_id, flow_version, person_id, state, \
            node, occurrence_id, state_version, trace_id, cause_kind, cause_id) \
         VALUES ($3, $2, $1, 1, $4, 'waiting', 'n', gen_random_uuid(), 1, gen_random_uuid(), \
            'api', 't')",
        "INSERT INTO step_executions (id, workspace_id, enrollment_id, node_id, outcome) \
         VALUES (gen_random_uuid(), $2, $3, 'n', 'waiting')",
        "INSERT INTO enrollment_waits (workspace_id, enrollment_id, person_id, event_name, \
            wake_version, timeout_at) VALUES ($2, $3, $4, 'seen', 1, now())",
        "INSERT INTO flow_entries (workspace_id, flow_id, person_id, entries, last_entered_at) \
         VALUES ($2, $1, $4, 1, now())",
        "INSERT INTO jobs (id, workspace_id, kind, payload, unique_key, cause_kind, cause_id, trace_id) \n         VALUES (gen_random_uuid(), $2, 'flow.step', \n            jsonb_build_object('enrollment_id', $3::text), 'seed:step', 'api', 't', \n            gen_random_uuid())",
        "INSERT INTO jobs (id, workspace_id, kind, payload, unique_key, cause_kind, cause_id, trace_id) \n         VALUES (gen_random_uuid(), $2, 'webhook.call', \n            jsonb_build_object('person_id', $4::text, 'enrollment_id', $3::text), \n            'seed:hook', 'api', 't', gen_random_uuid())",
        "INSERT INTO jobs (id, workspace_id, kind, payload, unique_key, cause_kind, cause_id, trace_id) \n         VALUES (gen_random_uuid(), $2, 'delivery.send', \n            jsonb_build_object('person_id', $4::text, 'enrollment_id', $3::text), \n            'seed:send', 'api', 't', gen_random_uuid())",
        "INSERT INTO jobs (id, workspace_id, kind, payload, unique_key, cause_kind, cause_id, trace_id) \n         VALUES (gen_random_uuid(), $2, 'delivery.sync', \n            jsonb_build_object('person_id', $4::text, 'enrollment_id', $3::text), \n            'seed:sync', 'api', 't', gen_random_uuid())",
        "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, template_id, \n            destination_id, status, reason, completed_at) \n         VALUES (gen_random_uuid(), $2, $4, $3, 'n', gen_random_uuid(), gen_random_uuid(), \n            'skipped', 'no_consent', now())",
        "INSERT INTO jobs (id, workspace_id, kind, payload, state, lease_token, lease_expires_at, \n            attempts, unique_key, cause_kind, cause_id, trace_id) \n         VALUES (gen_random_uuid(), $2, 'flow.step', \n            jsonb_build_object('enrollment_id', $3::text), 'running', gen_random_uuid(), \n            now() + interval '1 hour', 1, 'seed:running', 'api', 't', gen_random_uuid())",
    ] {
        sqlx::query(statement)
            .bind(flow)
            .bind(ws.as_uuid())
            .bind(enrollment)
            .bind(person)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    // The merged person kept an enrollment of their own in the same flow, with its rows.
    let merged_enrollment = Uuid::now_v7();
    for statement in [
        "INSERT INTO enrollments (id, workspace_id, flow_id, flow_version, person_id, state, \
            node, occurrence_id, state_version, trace_id, cause_kind, cause_id) \
         VALUES ($3, $2, $1, 1, $4, 'waiting', 'n', gen_random_uuid(), 1, gen_random_uuid(), \
            'api', 't')",
        "INSERT INTO step_executions (id, workspace_id, enrollment_id, node_id, outcome) \
         VALUES (gen_random_uuid(), $2, $3, 'n', 'waiting')",
        "INSERT INTO enrollment_waits (workspace_id, enrollment_id, person_id, event_name, \
            wake_version, timeout_at) VALUES ($2, $3, $4, 'seen', 1, now())",
        "INSERT INTO flow_entries (workspace_id, flow_id, person_id, entries, last_entered_at) \
         VALUES ($2, $1, $4, 1, now())",
        "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, template_id, \
            destination_id, status, reason, completed_at) \
         VALUES (gen_random_uuid(), $2, $4, $3, 'n', gen_random_uuid(), gen_random_uuid(), \
            'skipped', 'suppressed', now())",
    ] {
        sqlx::query(statement)
            .bind(flow)
            .bind(ws.as_uuid())
            .bind(merged_enrollment)
            .bind(merged)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    commit(tx).await.unwrap();

    // A template that keeps the person, the merged person and the bystander as preview people.
    let template = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO templates (id, workspace_id, name, channel, category) \
         VALUES ($1, $2, 'Seeded', 'email', 'marketing')",
    )
    .bind(template)
    .bind(ws.as_uuid())
    .execute(pool)
    .await
    .unwrap();
    for saved in [person, merged, bystander] {
        sqlx::query(
            "INSERT INTO template_preview_people (template_id, workspace_id, person_id) \
             VALUES ($1, $2, $3)",
        )
        .bind(template)
        .bind(ws.as_uuid())
        .bind(saved)
        .execute(pool)
        .await
        .unwrap();
    }

    // A conflict between the bystander and the person.
    let bystander_record: Uuid = sqlx::query_scalar(
        "SELECT id FROM source_records WHERE workspace_id = $1 AND record_key = '2'",
    )
    .bind(ws.as_uuid())
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO identity_conflicts \
            (id, workspace_id, person_id, other_person_id, kind, source_record_id, reason) \
         VALUES ($1, $2, $3, $4, 'email', $5, 'unique_kind')",
    )
    .bind(Uuid::now_v7())
    .bind(ws.as_uuid())
    .bind(bystander)
    .bind(person)
    .bind(bystander_record)
    .execute(pool)
    .await
    .unwrap();

    // A segment holding the person and the bystander, and the merged person's closed row.
    let segment = Uuid::now_v7();
    let mut tx = begin(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO segments (id, workspace_id, name, current_version, member_count) \
         VALUES ($1, $2, 'Everyone', 1, 2)",
    )
    .bind(segment)
    .bind(ws.as_uuid())
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO segment_versions (workspace_id, segment_id, version, definition, normalized, \
            deps, time_sensitive, trace_id) \
         VALUES ($1, $2, 1, '{}', '{}', '{}', false, $3)",
    )
    .bind(ws.as_uuid())
    .bind(segment)
    .bind(Uuid::now_v7())
    .execute(&mut *tx)
    .await
    .unwrap();
    for (member, closed) in [(person, false), (bystander, false), (merged, true)] {
        sqlx::query(
            "INSERT INTO segment_members (workspace_id, segment_id, person_id, version, \
                entered_at, entered_trace_id, entered_cause, exited_version, exited_at, \
                exited_trace_id, exited_cause) \
             VALUES ($1, $2, $3, 1, now(), $4, 'initial', \
                     CASE WHEN $5 THEN 1 END, CASE WHEN $5 THEN now() END, \
                     CASE WHEN $5 THEN $4 END, CASE WHEN $5 THEN 'data_change' END)",
        )
        .bind(ws.as_uuid())
        .bind(segment)
        .bind(member)
        .bind(Uuid::now_v7())
        .bind(closed)
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    commit(tx).await.unwrap();

    // A pending export.
    sqlx::query(
        "INSERT INTO privacy_exports (id, workspace_id, person_id, trace_id) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(Uuid::now_v7())
    .bind(ws.as_uuid())
    .bind(person)
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .unwrap();

    // Forwarding outbox rows (a pending, a dead and a delivered one) of the survivor, the merged
    // person and the bystander. Their payloads hold what a mapping took from an event.
    let destination = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO destinations (id, workspace_id, kind, name, config) \
         VALUES ($1, $2, 'fake', 'seeded sink', '{}')",
    )
    .bind(destination)
    .bind(ws.as_uuid())
    .execute(pool)
    .await
    .unwrap();
    for (holder, state) in [
        (person, "pending"),
        (person, "dead"),
        (merged, "pending"),
        (bystander, "delivered"),
    ] {
        sqlx::query(
            "INSERT INTO forwarding_outbox (id, workspace_id, destination_id, rule_id, event_id, \
                person_id, idempotency_key, payload, state) \
             VALUES (gen_random_uuid(), $1, $2, gen_random_uuid(), gen_random_uuid(), $3, \
                     'k', '{\"who\": \"seeded\"}', $4)",
        )
        .bind(ws.as_uuid())
        .bind(destination)
        .bind(holder)
        .bind(state)
        .execute(pool)
        .await
        .unwrap();
    }

    // A handled marker the retention left for the survivor at that destination.
    sqlx::query(
        "INSERT INTO erasure_handled_markers (workspace_id, person_id, destination_id)          VALUES ($1, $2, $3)",
    )
    .bind(ws.as_uuid())
    .bind(person)
    .bind(destination)
    .execute(pool)
    .await
    .unwrap();

    // Undrained inbox rows: the person's, spelled differently, and the bystander's.
    let item = |email: &str, message_id: &str| IngestItem {
        identifiers: vec![("email".into(), email.into())],
        timestamp: None,
        message_id: Some(message_id.into()),
        payload: IngestPayload::Identify {
            traits: json!({"plan": "late"}).as_object().cloned().unwrap(),
        },
    };
    append_inbox(
        pool,
        ws,
        Uuid::now_v7(),
        &trace(),
        &[
            item(" ADA@Example.com ", "i-1"),
            item(BYSTANDER_EMAIL, "i-2"),
        ],
    )
    .await
    .unwrap();

    let partitions: i64 =
        sqlx::query_scalar("SELECT count(DISTINCT tableoid) FROM events WHERE workspace_id = $1")
            .bind(ws.as_uuid())
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(partitions, 2, "the seeded events sit in two partitions");

    Seeded {
        ws,
        person,
        merged,
        bystander,
    }
}

/// What the export reads besides [`seed_everything`]'s rows: a tracked link and a recipient hash
/// on the survivor's message, a bystander message, and decision records and rejects of each person.
pub(crate) async fn seed_export_extras(pool: &PgPool, seeded: &Seeded) {
    let Seeded {
        ws,
        person,
        merged,
        bystander,
    } = *seeded;
    sqlx::query(
        "INSERT INTO message_links (workspace_id, message_id, link_index, url) \
         SELECT workspace_id, id, 0, 'https://example.com/seeded-link' FROM messages \
         WHERE workspace_id = $1 AND person_id = $2",
    )
    .bind(ws.as_uuid())
    .bind(person)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE messages SET address_hash = decode(repeat('ab', 32), 'hex') \
         WHERE workspace_id = $1 AND person_id = $2",
    )
    .bind(ws.as_uuid())
    .bind(person)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages (id, workspace_id, person_id, enrollment_id, node_id, template_id, \
            destination_id, status, reason, completed_at) \
         VALUES (gen_random_uuid(), $1, $2, gen_random_uuid(), 'bystander-node', \
                 gen_random_uuid(), gen_random_uuid(), 'skipped', 'no_consent', now())",
    )
    .bind(ws.as_uuid())
    .bind(bystander)
    .execute(pool)
    .await
    .unwrap();
    for (holder, marker) in [
        (person, "survivor-decision"),
        (merged, "merged-decision"),
        (bystander, "bystander-decision"),
    ] {
        sqlx::query(
            "INSERT INTO decision_records (id, workspace_id, trace_id, action, outcome, \
                cause_kind, cause_id, subject_ids) \
             VALUES (gen_random_uuid(), $1, gen_random_uuid(), $3, 'done', 'api', 't', \
                     jsonb_build_object('person', $2::text))",
        )
        .bind(ws.as_uuid())
        .bind(holder)
        .bind(marker)
        .execute(pool)
        .await
        .unwrap();
    }
    for (holder, marker) in [(person, "survivor-reject"), (bystander, "bystander-reject")] {
        sqlx::query(
            "INSERT INTO ingest_rejects (id, workspace_id, received_at, source, kind, reason, \
                payload, person_id, trace_id) \
             VALUES (gen_random_uuid(), $1, now(), 'web', 'event', 'schema', \
                     jsonb_build_object('marker', $3::text), $2, gen_random_uuid())",
        )
        .bind(ws.as_uuid())
        .bind(holder)
        .bind(marker)
        .execute(pool)
        .await
        .unwrap();
    }
}

/// The rows, per table with a `source_record_id` column, that name one of `records`. A table that
/// gains such a column is scanned without a change here.
pub(crate) async fn rows_naming_records(pool: &PgPool, records: &[Uuid]) -> Vec<(String, i64)> {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.columns \
         WHERE table_schema = current_schema() AND column_name = 'source_record_id' \
           AND data_type = 'uuid' \
           AND NOT EXISTS (SELECT 1 FROM pg_class c \
                           JOIN pg_namespace n ON n.oid = c.relnamespace \
                           WHERE n.nspname = table_schema::text \
                             AND c.relname = table_name::text AND c.relispartition) \
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let mut found = Vec::new();
    for table in tables {
        // Names come from the catalog, never from input.
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE source_record_id = ANY($1)"
        )))
        .bind(records)
        .fetch_one(pool)
        .await
        .unwrap();
        found.push((table, count));
    }
    found
}

/// How long a test waits for a condition before it fails, and how often it looks.
const POLL_CAP: std::time::Duration = std::time::Duration::from_secs(10);
const POLL_EVERY: std::time::Duration = std::time::Duration::from_millis(20);

/// Polls `condition` every 20 ms until it holds, and panics with `what` after 10 s.
pub(crate) async fn poll_until<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let started = std::time::Instant::now();
    while !condition().await {
        assert!(started.elapsed() < POLL_CAP, "timed out waiting for {what}");
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// Waits until the backend `pid` waits for a lock held by another transaction: the barrier that
/// orders transactions in the tests, in place of a guessed pause.
pub(crate) async fn wait_until_blocked(pool: &PgPool, pid: i32) {
    poll_until("the backend to wait for a lock", || async move {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
             WHERE pid = $1 AND wait_event_type = 'Lock')",
        )
        .bind(pid)
        .fetch_one(pool)
        .await
        .unwrap()
    })
    .await;
}

/// Waits until the backend `pid` waits for a lock held by the backend `blocker`.
pub(crate) async fn wait_until_blocked_by(pool: &PgPool, pid: i32, blocker: i32) {
    poll_until("the backend to wait for the other backend", || async move {
        sqlx::query_scalar("SELECT $2 = ANY(pg_blocking_pids($1))")
            .bind(pid)
            .bind(blocker)
            .fetch_one(pool)
            .await
            .unwrap()
    })
    .await;
}

/// Waits until the lease of the job has run out by the database's clock.
pub(crate) async fn wait_until_lease_expired(pool: &PgPool, id: Uuid) {
    poll_until("the lease to expire", || async move {
        sqlx::query_scalar("SELECT lease_expires_at < now() FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap()
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::people::erasure::rows_referencing_person;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_seed_touches_every_person_scoped_table(pool: PgPool) {
        let Seeded { person, merged, .. } = seed_everything(&pool).await;

        let places: Vec<String> = rows_referencing_person(&pool, person)
            .await
            .into_iter()
            .chain(rows_referencing_person(&pool, merged).await)
            .map(|(place, _)| place)
            .collect();

        // The debugger feed names no person; the scan reaches it through events and rejects.
        for place in ["debugger_feed.event_id", "debugger_feed.reject_id"] {
            assert!(places.contains(&place.to_owned()), "{place}: {places:?}");
        }
        // Every table the scan knows holds a person is represented in the seed.
        let known: Vec<(String,)> = sqlx::query_as(
            "SELECT table_name::text || '.' || column_name::text \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() AND data_type = 'uuid' \
               AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id') \
               AND table_name <> 'erasure_requests' \
               AND NOT EXISTS (SELECT 1 FROM pg_class c \
                               JOIN pg_namespace n ON n.oid = c.relnamespace \
                               WHERE n.nspname = table_schema::text \
                                 AND c.relname = table_name::text AND c.relispartition)",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        for (column,) in known {
            let covered = places.contains(&column)
                // The merge columns name the merged person on one side and the survivor on
                // the other, and a conflict names the survivor on one of its two columns.
                || column.starts_with("person_merges.")
                || column.starts_with("identity_conflicts.");
            assert!(covered, "{column} is not covered by the seed: {places:?}");
        }
    }
}
