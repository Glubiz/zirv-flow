//! The erasure scan scaffold (architecture section 7.4): finds every row that still refers to
//! a person. The erasure issue asserts that it returns nothing after an erasure; until then a
//! test shows it finds what the profile store writes.

use sqlx::PgPool;
use uuid::Uuid;

use crate::decisions::ID_SUBJECTS;

/// `(table.column, row count)` for every place that holds `person`, sorted by name. It scans
/// each column named `person_id` or ending in `_person_id`, `people.id`,
/// `people.merged_into`, and the subjects of `decision_records`. A partition is counted
/// through its parent table. Places with no row are left
/// out, so an empty result means the person is gone.
///
/// `erasure_requests` is skipped by name: the erasure ledger is the one table that
/// intentionally keeps the person id of an erased person, so that a restore can be erased
/// again (architecture section 16.3).
pub(crate) async fn rows_referencing_person(pool: &PgPool, person: Uuid) -> Vec<(String, i64)> {
    let mut found = Vec::new();
    for (table, column) in person_reference_places(pool).await {
        if (table.as_str(), column.as_str()) == DECISION_SUBJECTS {
            continue;
        }
        // Names come from the catalog and this module, never from input.
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE {column} = $1"
        )))
        .bind(person)
        .fetch_one(pool)
        .await
        .unwrap();
        if count > 0 {
            found.push((format!("{table}.{column}"), count));
        }
    }
    let decisions: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM decision_records d WHERE EXISTS \
            (SELECT 1 FROM {ID_SUBJECTS} AND s.value = $1::text)"
    )))
    .bind(person)
    .fetch_one(pool)
    .await
    .unwrap();
    if decisions > 0 {
        found.push(("decision_records.subject_ids".into(), decisions));
    }
    // The debugger feed names no person: its rows point at the person's events and rejects.
    for column in ["event_id", "reject_id"] {
        let table = if column == "event_id" {
            "events"
        } else {
            "ingest_rejects"
        };
        let fed: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM debugger_feed f WHERE f.{column} IN                 (SELECT id FROM {table} WHERE person_id = $1)"
        )))
        .bind(person)
        .fetch_one(pool)
        .await
        .unwrap();
        if fed > 0 {
            found.push((format!("debugger_feed.{column}"), fed));
        }
    }
    found.sort();
    found
}

/// The `(table, column)` of decision records' subject ids, the one place a person is named inside
/// a JSON column.
const DECISION_SUBJECTS: (&str, &str) = ("decision_records", "subject_ids");

/// Every `(table, column)` that can hold a person, sorted: each column named `person_id` or
/// ending in `_person_id` (a partition counts through its parent, `erasure_requests` is skipped),
/// plus `people.id`, `people.merged_into` and `decision_records.subject_ids`. The erasure scan and
/// the export catalog both read this list, so neither can drift from the other.
pub(crate) async fn person_reference_places(pool: &PgPool) -> Vec<(String, String)> {
    let mut places: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text FROM information_schema.columns \
         WHERE table_schema = current_schema() AND data_type = 'uuid' \
           AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id') \
           AND table_name <> 'erasure_requests' \
           AND NOT EXISTS (SELECT 1 FROM pg_class c \
                           JOIN pg_namespace n ON n.oid = c.relnamespace \
                           WHERE n.nspname = table_schema::text \
                             AND c.relname = table_name::text AND c.relispartition) \
         ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    places.push(("people".into(), "id".into()));
    places.push(("people".into(), "merged_into".into()));
    places.push((DECISION_SUBJECTS.0.into(), DECISION_SUBJECTS.1.into()));
    places.sort();
    places
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;
    use zirv_flow_core::{Cause, Slug, TraceContext, WorkspaceName};

    use super::*;
    use crate::create_workspace;
    use crate::people::{ProfileOutcome, ProfileRecord, apply_profile_batch};

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_scan_finds_the_rows_the_profile_store_wrote(pool: PgPool) {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        crate::segments::tests_support::segment_reading(
            &pool,
            ws,
            zirv_flow_core::segment::ChangeClasses::TRAITS,
        )
        .await;
        let record = ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: "1".into(),
            identifiers: vec![("email".into(), "a@example.com".into())],
            traits: json!({"plan": "pro"}).as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("t")),
        };
        let outcomes = apply_profile_batch(&pool, ws, &[record]).await.unwrap();
        let ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };

        let found = rows_referencing_person(&pool, person_id).await;

        let names: Vec<&str> = found.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "decision_records.subject_ids",
                "identities.person_id",
                "people.id",
                "person_changes.person_id",
                "source_records.person_id",
            ]
        );
        assert!(
            rows_referencing_person(&pool, Uuid::now_v7())
                .await
                .is_empty()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_scan_finds_the_merge_provenance_of_both_people(pool: PgPool) {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let record = |key: &str, identifiers: &[(&str, &str)]| ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: key.into(),
            identifiers: identifiers
                .iter()
                .map(|(kind, value)| ((*kind).into(), (*value).into()))
                .collect(),
            traits: json!({}).as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("t")),
        };
        let outcomes = apply_profile_batch(
            &pool,
            ws,
            &[
                record("1", &[("external:crm", "1")]),
                record("2", &[("anonymous_id", "a")]),
                record("3", &[("external:crm", "1"), ("anonymous_id", "a")]),
            ],
        )
        .await
        .unwrap();
        let ProfileOutcome::Applied {
            person_id: survivor,
            ..
        } = outcomes[0]
        else {
            panic!("not applied");
        };
        let ProfileOutcome::Applied {
            person_id: merged, ..
        } = outcomes[1]
        else {
            panic!("not applied");
        };

        let for_survivor = rows_referencing_person(&pool, survivor).await;
        let for_merged = rows_referencing_person(&pool, merged).await;

        assert!(for_survivor.contains(&("person_merges.survivor_person_id".to_owned(), 1)));
        assert!(for_merged.contains(&("person_merges.merged_person_id".to_owned(), 1)));
        assert!(for_merged.contains(&("people.id".to_owned(), 1)));
        assert!(
            for_merged
                .iter()
                .all(|(place, _)| place != "people.merged_into")
        );
        assert!(for_survivor.contains(&("people.merged_into".to_owned(), 1)));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_scan_finds_the_rows_the_event_store_wrote(pool: PgPool) {
        use crate::{
            EventOutcome, EventPolicy, EventRecord, HmacPurpose, HmacRoot, MasterKey,
            apply_event_batch, set_event_policy,
        };
        use zirv_flow_core::events::UnplannedPolicy;

        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        crate::segments::tests_support::segment_reading(
            &pool,
            ws,
            zirv_flow_core::segment::ChangeClasses::EVENTS,
        )
        .await;
        let record = |event: &str, message_id: &str| EventRecord {
            source_record_id: None,
            source: "web".into(),
            identifiers: vec![("email".into(), "a@example.com".into())],
            name: event.into(),
            properties: json!({"plan": "pro"}).as_object().cloned().unwrap(),
            timestamp: None,
            received_at: datetime!(2026-01-01 00:00 UTC),
            message_id: Some(message_id.into()),
            trace: TraceContext::root(Cause::api("t")),
        };
        let key = HmacRoot::bootstrap_from(&MasterKey::generate()).hmac_key(HmacPurpose::Redaction);
        let accepted = apply_event_batch(&pool, ws, &key, &[record("seen", "m-1")])
            .await
            .unwrap();
        let EventOutcome::Accepted { person_id, .. } = accepted[0] else {
            panic!("not accepted");
        };
        set_event_policy(
            &pool,
            ws,
            &EventPolicy {
                unplanned: UnplannedPolicy::Reject,
                ..EventPolicy::default()
            },
        )
        .await
        .unwrap();
        apply_event_batch(&pool, ws, &key, &[record("unknown", "m-2")])
            .await
            .unwrap();

        let found = rows_referencing_person(&pool, person_id).await;

        let names: Vec<&str> = found.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "decision_records.subject_ids",
                "event_dedupe.person_id",
                "events.person_id",
                "identities.person_id",
                "ingest_rejects.person_id",
                "people.id",
                "person_changes.person_id",
            ]
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_scan_finds_list_memberships_history_and_decisions(pool: PgPool) {
        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let record = ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: "1".into(),
            identifiers: vec![("email".into(), "a@example.com".into())],
            traits: json!({}).as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("t")),
        };
        let outcomes = apply_profile_batch(&pool, ws, &[record]).await.unwrap();
        let ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };
        let list = crate::create_list(&pool, ws, "VIPs", "", None)
            .await
            .unwrap();
        let mut tx = crate::begin(&pool).await.unwrap();
        crate::add_members(
            &mut tx,
            ws,
            list.id,
            &[person_id],
            crate::ListSource::Api,
            None,
            &TraceContext::root(Cause::api("t")),
        )
        .await
        .unwrap();
        crate::commit(tx).await.unwrap();

        let found = rows_referencing_person(&pool, person_id).await;

        for place in ["list_members.person_id", "list_member_changes.person_id"] {
            assert!(found.contains(&(place.to_owned(), 1)), "{place}: {found:?}");
        }
        let decisions = found
            .iter()
            .find(|(place, _)| place == "decision_records.subject_ids")
            .map(|(_, count)| *count);
        assert!(decisions >= Some(2), "{found:?}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_scan_finds_consent_and_its_history_but_not_suppressions(pool: PgPool) {
        use zirv_flow_core::consent::{ChangeSource, Channel, ConsentStatus, SuppressionReason};

        use crate::{ConsentChange, NewSuppression, add_suppression, begin, commit, set_consent};

        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let record = ProfileRecord {
            source: "crm".into(),
            stream: "contacts".into(),
            record_key: "1".into(),
            identifiers: vec![("email".into(), "a@example.com".into())],
            traits: json!({}).as_object().cloned().unwrap(),
            observed_at: datetime!(2026-01-01 00:00 UTC),
            trace: TraceContext::root(Cause::api("t")),
        };
        let outcomes = apply_profile_batch(&pool, ws, &[record]).await.unwrap();
        let ProfileOutcome::Applied { person_id, .. } = outcomes[0] else {
            panic!("not applied");
        };
        let mut tx = begin(&pool).await.unwrap();
        set_consent(
            &mut tx,
            ws,
            &ConsentChange {
                dimensions: Default::default(),
                person: person_id,
                channel: Channel::Email,
                topic: None,
                status: ConsentStatus::Subscribed,
                source: ChangeSource::Api,
                evidence: json!({}),
                occurred_at: datetime!(2026-01-01 00:00 UTC),
                actor: None,
            },
            &TraceContext::root(Cause::api("t")),
        )
        .await
        .unwrap();
        commit(tx).await.unwrap();
        add_suppression(
            &pool,
            ws,
            &NewSuppression {
                identifier_hash: [1; 32],
                kind: "email",
                reason: SuppressionReason::Erased,
                source: "erasure",
                note: None,
                created_by: None,
            },
        )
        .await
        .unwrap();

        let found = rows_referencing_person(&pool, person_id).await;

        let names: Vec<&str> = found.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"consent.person_id"));
        assert!(names.contains(&"consent_changes.person_id"));
        assert!(names.contains(&"decision_records.subject_ids"));
        assert!(names.iter().all(|name| !name.starts_with("suppressions")));
    }
}
