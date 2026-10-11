//! The erasure scan scaffold (architecture section 7.4): finds every row that still refers to
//! a person. The erasure issue asserts that it returns nothing after an erasure; until then a
//! test shows it finds what the profile store writes.

use sqlx::PgPool;
use uuid::Uuid;

/// `(table.column, row count)` for every place that holds `person`, sorted by name. It scans
/// each column named `person_id` or ending in `_person_id`, `people.id`,
/// `people.merged_into`, and the subjects of `decision_records`. A partition is counted
/// through its parent table. Places with no row are left
/// out, so an empty result means the person is gone.
pub(crate) async fn rows_referencing_person(pool: &PgPool, person: Uuid) -> Vec<(String, i64)> {
    let mut places: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text FROM information_schema.columns \
         WHERE table_schema = current_schema() AND data_type = 'uuid' \
           AND (column_name = 'person_id' OR column_name LIKE '%\\_person\\_id') \
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

    let mut found = Vec::new();
    for (table, column) in places {
        // Names come from the catalog and this function, never from input.
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
    let decisions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM decision_records d WHERE EXISTS \
            (SELECT 1 FROM jsonb_each_text(d.subject_ids) s WHERE s.value = $1::text)",
    )
    .bind(person)
    .fetch_one(pool)
    .await
    .unwrap();
    if decisions > 0 {
        found.push(("decision_records.subject_ids".into(), decisions));
    }
    found.sort();
    found
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
            EventOutcome, EventPolicy, EventRecord, HmacPurpose, MasterKey, apply_event_batch,
            set_event_policy,
        };
        use zirv_flow_core::events::UnplannedPolicy;

        let name = WorkspaceName::parse("one").unwrap();
        let slug = Slug::parse("one").unwrap();
        let ws = create_workspace(&pool, &name, &slug).await.unwrap().id;
        let record = |event: &str, message_id: &str| EventRecord {
            source: "web".into(),
            identifiers: vec![("email".into(), "a@example.com".into())],
            name: event.into(),
            properties: json!({"plan": "pro"}).as_object().cloned().unwrap(),
            timestamp: None,
            received_at: datetime!(2026-01-01 00:00 UTC),
            message_id: Some(message_id.into()),
            trace: TraceContext::root(Cause::api("t")),
        };
        let key = MasterKey::generate().hmac_key(HmacPurpose::Redaction);
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
}
