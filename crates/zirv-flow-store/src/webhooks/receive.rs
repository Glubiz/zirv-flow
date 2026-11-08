//! The receive path of a hook: find it by the URL id, then append a verified payload to the
//! inbox. The signature is checked by the caller between the two; nothing is written before.

use hashbrown::HashMap;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;
use zirv_flow_core::mapping::{Mapping, Target, TraitState, map_json};
use zirv_flow_core::{TraceContext, WorkspaceId};

use crate::ingest::{Shard, bucket, drain_shards, enqueue_drain_shard, strongest_identifier};
use crate::{
    PgPool, StoreError, StreamOutcome, begin, commit, database_now, record_stream_outcome,
};

/// What the receive path needs to know about a hook.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct HookReceive {
    pub id: Uuid,
    pub workspace_id: Uuid,
    pub source_id: Uuid,
    pub stream_id: Uuid,
    pub secret_id: Uuid,
    pub signature_header: String,
    pub signature_prefix: String,
    pub timestamp_header: Option<String>,
    pub tolerance_seconds: i32,
    pub dedupe_field: String,
    /// The source and the stream are enabled and the stream is mapped.
    pub receiving: bool,
    /// The stream's mapping as stored, which buckets a payload by its person.
    pub mapping: Option<Value>,
}

impl HookReceive {
    /// The hook's workspace.
    pub fn workspace(&self) -> WorkspaceId {
        WorkspaceId::from_uuid(self.workspace_id)
    }
}

/// The hook with this URL id, in whichever workspace it lives: one join over the hook, its
/// source and its stream. The id only locates the hook (the HMAC authorizes), so there is no
/// workspace filter; the result carries the workspace.
pub async fn find_for_receive(pool: &PgPool, id: Uuid) -> Result<Option<HookReceive>, StoreError> {
    sqlx::query_as(
        "SELECT w.id, w.workspace_id, w.source_id, w.stream_id, w.secret_id, \
                w.signature_header, w.signature_prefix, w.timestamp_header, \
                w.tolerance_seconds, w.dedupe_field, \
                (s.enabled AND t.enabled AND t.mapping IS NOT NULL) AS receiving, t.mapping \
         FROM webhooks w \
         JOIN sources s ON s.workspace_id = w.workspace_id AND s.id = w.source_id \
         JOIN streams t ON t.workspace_id = w.workspace_id AND t.id = w.stream_id \
         WHERE w.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(StoreError::query)
}

/// The inbox bucket of a payload: that of the record key the drain will apply it under, the
/// strongest identifier the identifier columns of the hook's mapping find in it. So one person's
/// payloads share a bucket, and a drain lane, whichever hooks they come through, and they share
/// it with the API's rows of the same key; the people of one hook spread over the lanes. The
/// identifier columns run alone, through the function the drain maps with, so the key is the
/// drain's whatever the other columns hold. A payload the mapping finds no identifier in now,
/// or a mapping that does not read, is bucketed by the hook: the drain refuses such a row unless
/// the mapping is changed first, and a refused row has no person to keep in order.
fn payload_bucket(hook: Uuid, mapping: Option<&Value>, payload: &Value) -> i16 {
    mapping
        .and_then(|mapping| Mapping::deserialize(mapping).ok())
        .and_then(|mapping| {
            let identifying = Mapping {
                columns: mapping
                    .columns
                    .into_iter()
                    .filter(|rule| matches!(rule.target, Target::Identifier { .. }))
                    .collect(),
                ..Mapping::default()
            };
            let catalog: HashMap<String, TraitState> = HashMap::new();
            let mapped = map_json(&identifying, payload, identifying.root_fields(), &catalog);
            strongest_identifier(&mapped.ok()?.identifiers)
        })
        .map_or_else(|| bucket(&hook.to_string()), |key| bucket(&key))
}

/// Appends one verified payload to the inbox, enqueues the drain of its bucket's shard and, when `report`
/// is set, reports the stream as succeeded now, all in one transaction. The row is stamped
/// `received_at` with the database's clock and bucketed by its person ([`payload_bucket`]).
/// `message_id` is the namespaced dedupe value (`wh:{hook}:{value}`); `signed_at` is the signed
/// timestamp of the timestamped scheme.
pub async fn append_webhook(
    pool: &PgPool,
    hook: &HookReceive,
    trace: &TraceContext,
    item: &Value,
    signed_at: Option<OffsetDateTime>,
    message_id: &str,
    report: bool,
) -> Result<(), StoreError> {
    let stored = json!({
        "payload": item,
        "signed_at": signed_at.and_then(|at| at.format(&Rfc3339).ok()),
    });
    let workspace = hook.workspace();
    let affinity = payload_bucket(hook.id, hook.mapping.as_ref(), item);
    let mut tx = begin(pool).await?;
    sqlx::query(
        "INSERT INTO ingest_inbox \
            (workspace_id, webhook_id, kind, item, message_id, received_at, trace_id, affinity) \
         VALUES ($1, $2, 'webhook', $3, $4, now(), $5, $6)",
    )
    .bind(workspace.as_uuid())
    .bind(hook.id)
    .bind(&stored)
    .bind(message_id)
    .bind(trace.trace_id.as_uuid())
    .bind(affinity)
    .execute(&mut *tx)
    .await
    .map_err(StoreError::query)?;
    enqueue_drain_shard(
        &mut *tx,
        workspace,
        Shard::of_bucket(affinity, drain_shards()),
        trace,
    )
    .await?;
    if report {
        let at = database_now(&mut *tx).await?;
        record_stream_outcome(
            &mut tx,
            workspace,
            hook.stream_id,
            StreamOutcome::Succeeded,
            at,
        )
        .await?;
    }
    commit(tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    type InboxShape = (String, Option<Uuid>, Option<Uuid>, i16, serde_json::Value);

    use hashbrown::HashSet;
    use serde_json::json;
    use sqlx::PgPool;

    use super::super::fixtures::{hook, mapping, trace, workspace};
    use super::*;
    use crate::{StreamSettings, configure_stream, find_stream_by_id};

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hook_is_found_by_its_id_and_receives_only_when_enabled_and_mapped(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mapped = hook(&pool, ws, "orders", Some(&mapping(&[]))).await;
        let unmapped = hook(&pool, ws, "forms", None).await;

        let found = find_for_receive(&pool, mapped.id).await.unwrap().unwrap();
        assert_eq!(found.workspace(), ws);
        assert_eq!(found.source_id, mapped.source_id);
        assert_eq!(found.dedupe_field, "id");
        assert!(found.receiving);
        assert!(
            !find_for_receive(&pool, unmapped.id)
                .await
                .unwrap()
                .unwrap()
                .receiving
        );
        assert!(
            find_for_receive(&pool, Uuid::new_v4())
                .await
                .unwrap()
                .is_none()
        );

        let mut connection = pool.acquire().await.unwrap();
        let off = StreamSettings {
            enabled: false,
            sync_mode: "full_refresh",
            cursor_field: None,
            primary_key: &[],
            staleness_threshold_seconds: None,
            reconcile_schedule: None,
            sweep_limit: 0.5,
        };
        configure_stream(&mut connection, ws, mapped.source_id, "payload", &off)
            .await
            .unwrap();
        assert!(
            !find_for_receive(&pool, mapped.id)
                .await
                .unwrap()
                .unwrap()
                .receiving
        );
        configure_stream(
            &mut connection,
            ws,
            mapped.source_id,
            "payload",
            &StreamSettings {
                enabled: true,
                ..off
            },
        )
        .await
        .unwrap();
        sqlx::query("UPDATE sources SET enabled = false WHERE id = $1")
            .bind(mapped.source_id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            !find_for_receive(&pool, mapped.id)
                .await
                .unwrap()
                .unwrap()
                .receiving
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_append_writes_one_webhook_row_and_one_drain_and_reports_only_when_asked(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let created = hook(&pool, ws, "orders", Some(&mapping(&[]))).await;
        let receive = find_for_receive(&pool, created.id).await.unwrap().unwrap();
        let item = json!({"id": "o-1", "email": "a@example.com"});

        append_webhook(&pool, &receive, &trace(), &item, None, "wh:x:o-1", false)
            .await
            .unwrap();
        append_webhook(&pool, &receive, &trace(), &item, None, "wh:x:o-2", false)
            .await
            .unwrap();

        let rows: Vec<InboxShape> = sqlx::query_as(
            "SELECT kind, webhook_id, api_key_id, affinity, item FROM ingest_inbox ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "webhook");
        assert_eq!(rows[0].1, Some(created.id));
        assert_eq!(rows[0].2, None);
        assert_eq!(rows[0].3, bucket("email:a@example.com"));
        assert_eq!(rows[0].4, json!({"payload": item, "signed_at": null}));
        let drains: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'ingest.drain'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(drains, 1);
        let stream = find_stream_by_id(&pool, ws, created.source_id, created.stream_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stream.last_success_at, None);

        append_webhook(&pool, &receive, &trace(), &item, None, "wh:x:o-3", true)
            .await
            .unwrap();
        let stream = find_stream_by_id(&pool, ws, created.source_id, created.stream_id)
            .await
            .unwrap()
            .unwrap();
        assert!(stream.last_success_at.is_some());
    }

    /// The bucket the API gives an identify row with `identifiers`.
    fn api_bucket(identifiers: &[(&str, &str)]) -> i16 {
        let pairs: Vec<(String, String)> = identifiers
            .iter()
            .map(|(kind, value)| ((*kind).to_owned(), (*value).to_owned()))
            .collect();
        bucket(&strongest_identifier(&pairs).unwrap())
    }

    #[test]
    fn a_payload_is_bucketed_by_the_strongest_identifier_its_mapping_finds_as_the_api_rows_are() {
        let (form, consent) = (Uuid::new_v4(), Uuid::new_v4());
        let list = Uuid::now_v7();
        // A form with a trait the bucket does not need the registry for, a list and an event.
        let form_mapping = json!({
            "columns": [
                {"column": "email", "target": {"type": "identifier", "kind": "email"},
                 "transforms": [{"op": "trim"}]},
                {"column": "plan", "target": {"type": "trait", "key": "plan"}},
                {"column": "subscribed", "target": {"type": "list", "list_id": list}}
            ],
            "event": {"name": "form_submitted"}
        });
        // A consent hook that reads a nested address and a stronger identifier when it has one.
        let consent_mapping = json!({
            "columns": [
                {"column": "contact.email", "target": {"type": "identifier", "kind": "email"}},
                {"column": "contact.crm", "target": {"type": "identifier", "kind": "external:crm"}}
            ]
        });
        let submitted = json!({"email": " ada@example.com ", "plan": "pro", "subscribed": "yes"});
        let changed = json!({"contact": {"email": "ada@example.com"}});

        let ada = api_bucket(&[("email", "ada@example.com")]);
        assert_eq!(payload_bucket(form, Some(&form_mapping), &submitted), ada);
        assert_eq!(
            payload_bucket(consent, Some(&consent_mapping), &changed),
            ada
        );
        let known = json!({"contact": {"email": "ada@example.com", "crm": "c-1"}});
        assert_eq!(
            payload_bucket(consent, Some(&consent_mapping), &known),
            api_bucket(&[("email", "ada@example.com"), ("external:crm", "c-1")]),
            "the strongest identifier, as for the API's rows"
        );

        let spread: HashSet<i16> = (0..32)
            .map(|n| {
                let payload = json!({"email": format!("person{n}@example.com")});
                payload_bucket(form, Some(&form_mapping), &payload)
            })
            .collect();
        assert!(spread.len() > 8, "one hook's people spread: {spread:?}");

        let by_hook = bucket(&form.to_string());
        let keyless = json!({"plan": "pro"});
        assert_eq!(payload_bucket(form, Some(&form_mapping), &keyless), by_hook);
        let invalid = json!({"email": "not an address"});
        assert_eq!(payload_bucket(form, Some(&form_mapping), &invalid), by_hook);
        let unreadable = json!({"columns": "no"});
        assert_eq!(payload_bucket(form, Some(&unreadable), &submitted), by_hook);
        assert_eq!(payload_bucket(form, None, &submitted), by_hook);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn one_persons_payloads_share_a_bucket_and_one_hooks_people_spread(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let created = hook(&pool, ws, "orders", Some(&mapping(&[]))).await;
        let receive = find_for_receive(&pool, created.id).await.unwrap().unwrap();
        let emails = [
            "ada@example.com",
            "bob@example.com",
            "ada@example.com",
            "cyd@example.com",
            "dan@example.com",
        ];
        for (n, email) in emails.iter().enumerate() {
            let item = json!({"id": n.to_string(), "email": email});
            let message = format!("wh:x:{n}");
            append_webhook(&pool, &receive, &trace(), &item, None, &message, false)
                .await
                .unwrap();
        }

        let stored: Vec<i16> = sqlx::query_scalar("SELECT affinity FROM ingest_inbox ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        let expected: Vec<i16> = emails
            .iter()
            .map(|email| api_bucket(&[("email", email)]))
            .collect();
        assert_eq!(stored, expected);
        assert_eq!(stored[0], stored[2], "Ada's payloads share a bucket");
        let distinct: HashSet<i16> = stored.iter().copied().collect();
        assert!(distinct.len() > 1, "the hook's people spread: {stored:?}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_origin_check_refuses_a_webhook_row_with_a_key_and_an_api_row_with_a_hook(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let created = hook(&pool, ws, "orders", None).await;
        let insert = |kind: &'static str, key: Option<Uuid>, hook: Option<Uuid>| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO ingest_inbox \
                        (workspace_id, api_key_id, webhook_id, kind, item, message_id, trace_id) \
                     VALUES ($1, $2, $3, $4, '{}'::jsonb, 'm', $5)",
                )
                .bind(ws.as_uuid())
                .bind(key)
                .bind(hook)
                .bind(kind)
                .bind(Uuid::now_v7())
                .execute(&pool)
                .await
            }
        };

        assert!(
            insert("webhook", Some(Uuid::now_v7()), Some(created.id))
                .await
                .is_err()
        );
        assert!(insert("webhook", None, None).await.is_err());
        assert!(
            insert("identify", Some(Uuid::now_v7()), Some(created.id))
                .await
                .is_err()
        );
        assert!(insert("identify", None, None).await.is_err());
        assert!(insert("webhook", None, Some(created.id)).await.is_ok());
        assert!(insert("identify", Some(Uuid::now_v7()), None).await.is_ok());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_a_hook_drops_its_queued_payloads(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let created = hook(&pool, ws, "orders", Some(&mapping(&[]))).await;
        let receive = find_for_receive(&pool, created.id).await.unwrap().unwrap();
        append_webhook(
            &pool,
            &receive,
            &trace(),
            &json!({"id": "1"}),
            None,
            "wh:x:1",
            false,
        )
        .await
        .unwrap();

        let mut connection = pool.acquire().await.unwrap();
        crate::delete_source(&mut connection, ws, created.source_id)
            .await
            .unwrap();

        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM ingest_inbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }
}
