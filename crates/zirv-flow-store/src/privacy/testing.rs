//! Test support: a workspace holding a person with every M2 data class.

use serde_json::json;
use sqlx::PgPool;
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceName;
use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus};
use zirv_flow_core::{Cause, HmacKey, HmacPurpose, MasterKey, Slug, TraceContext, WorkspaceId};

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
    master_key().hmac_key(HmacPurpose::Identifier)
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
    let key = master_key().hmac_key(HmacPurpose::Redaction);
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
