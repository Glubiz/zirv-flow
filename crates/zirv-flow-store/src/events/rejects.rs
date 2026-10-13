//! Reads of ingest rejects (architecture section 12.1). The stored payload is kept for
//! replay and is never selected here: a reject shows its masked sample only.

use serde_json::{Value, json};
use sqlx::prelude::FromRow;
use sqlx::types::Json;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::events::Reason;
use zirv_flow_core::{TraceContext, WorkspaceId};

use crate::StoreError;

/// A reject as the API shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct IngestReject {
    pub id: Uuid,
    pub received_at: OffsetDateTime,
    pub source: String,
    pub kind: String,
    pub name: Option<String>,
    pub reason: String,
    pub reasons: Vec<Reason>,
    /// `{properties, masked}`: values are masked as far as the rules require.
    pub sample: Value,
    pub person_id: Option<Uuid>,
    pub message_id: Option<String>,
    pub trace_id: Uuid,
    pub replayed_at: Option<OffsetDateTime>,
}

/// Where the previous page ended: the last reject it showed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RejectCursor {
    pub received_at: OffsetDateTime,
    pub id: Uuid,
}

/// Narrows a listing; both fields must match when set.
#[derive(Clone, Copy, Debug, Default)]
pub struct RejectFilter<'a> {
    pub name: Option<&'a str>,
    pub reason: Option<&'a str>,
}

#[derive(FromRow)]
struct RejectRow {
    id: Uuid,
    received_at: OffsetDateTime,
    source: String,
    kind: String,
    name: Option<String>,
    reason: String,
    reasons: Json<Vec<Reason>>,
    sample: Value,
    person_id: Option<Uuid>,
    message_id: Option<String>,
    trace_id: Uuid,
    replayed_at: Option<OffsetDateTime>,
}

/// The workspace's rejects, newest first, at most `limit`, after `before` when given. The
/// flag is true when more existed.
pub async fn list_ingest_rejects<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    filter: RejectFilter<'_>,
    before: Option<RejectCursor>,
    limit: u32,
) -> Result<(Vec<IngestReject>, bool), StoreError> {
    let rows: Vec<RejectRow> = sqlx::query_as(
        "SELECT id, received_at, source, kind, name, reason, reasons, sample, person_id, \
                message_id, trace_id, replayed_at \
         FROM ingest_rejects \
         WHERE workspace_id = $1 \
           AND ($2::text IS NULL OR name = $2) \
           AND ($3::text IS NULL OR reason = $3) \
           AND ($4::timestamptz IS NULL OR (received_at, id) < ($4, $5)) \
         ORDER BY received_at DESC, id DESC LIMIT $6",
    )
    .bind(workspace.as_uuid())
    .bind(filter.name)
    .bind(filter.reason)
    .bind(before.map(|cursor| cursor.received_at))
    .bind(before.map(|cursor| cursor.id))
    .bind(i64::from(limit) + 1)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    let truncated = rows.len() > limit as usize;
    let rejects = rows
        .into_iter()
        .take(limit as usize)
        .map(|row| IngestReject {
            id: row.id,
            received_at: row.received_at,
            source: row.source,
            kind: row.kind,
            name: row.name,
            reason: row.reason,
            reasons: row.reasons.0,
            sample: row.sample,
            person_id: row.person_id,
            message_id: row.message_id,
            trace_id: row.trace_id,
            replayed_at: row.replayed_at,
        })
        .collect();
    Ok((rejects, truncated))
}

/// A mapping refusal to store, with the codes that explain it.
pub(crate) struct Refusal {
    pub(crate) id: Uuid,
    /// `(field, code)` pairs; the field is empty when no one field is at fault.
    pub(crate) reasons: Vec<(String, &'static str)>,
    /// `{properties, masked}`: never a value outside the non-PII traits.
    pub(crate) sample: Value,
}

/// Keeps records the mapping or the store refused, as `record` rejects of the stream. A sync
/// run ties them to the run (`sync_run_id`); a webhook payload has no run. The payload is not
/// kept, only the masked sample.
pub(crate) async fn insert_record_rejects(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    source: &str,
    stream: &str,
    trace: &TraceContext,
    sync_run_id: Option<Uuid>,
    refusals: &[Refusal],
) -> Result<(), StoreError> {
    if refusals.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = refusals.iter().map(|refusal| refusal.id).collect();
    let codes: Vec<&str> = refusals
        .iter()
        .map(|refusal| {
            refusal
                .reasons
                .first()
                .map_or("rejected", |(_, code)| *code)
        })
        .collect();
    let reasons: Vec<Value> = refusals
        .iter()
        .map(|refusal| {
            Value::Array(
                refusal
                    .reasons
                    .iter()
                    .map(|(field, code)| json!({"property": field, "reason": code}))
                    .collect(),
            )
        })
        .collect();
    let samples: Vec<Value> = refusals
        .iter()
        .map(|refusal| refusal.sample.clone())
        .collect();
    sqlx::query(
        "INSERT INTO ingest_rejects \
            (id, workspace_id, received_at, source, kind, name, reason, reasons, sample, \
             payload, trace_id, sync_run_id) \
         SELECT t.id, $1, now(), $2, 'record', $3, t.reason, t.reasons, t.sample, \
                '{}'::jsonb, $4, $5 \
         FROM unnest($6::uuid[], $7::text[], $8::jsonb[], $9::jsonb[]) \
              AS t(id, reason, reasons, sample)",
    )
    .bind(workspace.as_uuid())
    .bind(source)
    .bind(stream)
    .bind(trace.trace_id.as_uuid())
    .bind(sync_run_id)
    .bind(ids)
    .bind(codes)
    .bind(reasons)
    .bind(samples)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration;
    use time::macros::datetime;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;

    const NOW: OffsetDateTime = datetime!(2026-06-01 12:00 UTC);

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    async fn insert(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        reason: &str,
        age_minutes: i64,
    ) -> Uuid {
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO ingest_rejects \
                (id, workspace_id, received_at, source, kind, name, reason, sample, payload, trace_id) \
             VALUES ($1, $2, $3, 'web', 'event', $4, $5, $6, $7, $8)",
        )
        .bind(id)
        .bind(ws.as_uuid())
        .bind(NOW - Duration::minutes(age_minutes))
        .bind(name)
        .bind(reason)
        .bind(json!({"properties": {"a": null}, "masked": ["a"]}))
        .bind(json!({"properties": {"a": "raw-secret"}}))
        .bind(Uuid::now_v7())
        .execute(pool)
        .await
        .unwrap();
        id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rejects_come_newest_first_in_pages_and_never_show_the_payload(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut ids = Vec::new();
        for age in [30, 20, 10] {
            ids.push(insert(&pool, ws, "order", "validation_failed", age).await);
        }

        let (first, more) = list_ingest_rejects(&pool, ws, RejectFilter::default(), None, 2)
            .await
            .unwrap();
        let cursor = RejectCursor {
            received_at: first[1].received_at,
            id: first[1].id,
        };
        let (second, more_after) =
            list_ingest_rejects(&pool, ws, RejectFilter::default(), Some(cursor), 2)
                .await
                .unwrap();

        assert!(more && !more_after);
        assert_eq!(
            first.iter().map(|r| r.id).collect::<Vec<_>>(),
            [ids[2], ids[1]]
        );
        assert_eq!(second.iter().map(|r| r.id).collect::<Vec<_>>(), [ids[0]]);
        let shown = format!("{first:?}{second:?}");
        assert!(!shown.contains("raw-secret"));
        assert_eq!(
            first[0].sample,
            json!({"properties": {"a": null}, "masked": ["a"]})
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_name_and_reason_filters_narrow_the_list(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        insert(&pool, ws, "order", "validation_failed", 3).await;
        insert(&pool, ws, "order", "unplanned_event", 2).await;
        insert(&pool, ws, "signup", "validation_failed", 1).await;

        let by_name = RejectFilter {
            name: Some("order"),
            reason: None,
        };
        let by_both = RejectFilter {
            name: Some("order"),
            reason: Some("unplanned_event"),
        };
        let (named, _) = list_ingest_rejects(&pool, ws, by_name, None, 10)
            .await
            .unwrap();
        let (both, _) = list_ingest_rejects(&pool, ws, by_both, None, 10)
            .await
            .unwrap();

        assert_eq!(named.len(), 2);
        assert_eq!(both.len(), 1);
        assert_eq!(both[0].reason, "unplanned_event");
    }
}
