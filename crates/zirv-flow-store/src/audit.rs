//! The append-only audit log. This module appends and reads; the only deleter is the database
//! function `audit_purge` (migration 0077), which [`purge_audit`] calls. A trigger refuses every
//! other UPDATE, DELETE and TRUNCATE.

use futures_util::Stream;
use futures_util::stream::try_unfold;
use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgExecutor, PgPool, Postgres, QueryBuilder};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{WorkspaceId, audit};

use crate::StoreError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActorKind {
    User,
    ApiKey,
    System,
    Anonymous,
}

impl ActorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ApiKey => "api_key",
            Self::System => "system",
            Self::Anonymous => "anonymous",
        }
    }
}

/// Who did it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Actor {
    pub kind: ActorKind,
    pub id: Option<Uuid>,
}

impl Actor {
    pub const fn user(id: Uuid) -> Self {
        Self {
            kind: ActorKind::User,
            id: Some(id),
        }
    }

    pub const fn api_key(id: Uuid) -> Self {
        Self {
            kind: ActorKind::ApiKey,
            id: Some(id),
        }
    }

    pub const fn system() -> Self {
        Self {
            kind: ActorKind::System,
            id: None,
        }
    }

    pub const fn anonymous() -> Self {
        Self {
            kind: ActorKind::Anonymous,
            id: None,
        }
    }
}

/// A row to append. The summary must never hold emails, passwords, tokens or key secrets.
#[derive(Clone, Debug)]
pub struct NewAuditEntry {
    pub workspace_id: Option<WorkspaceId>,
    pub actor: Actor,
    pub action: &'static str,
    pub object_kind: Option<&'static str>,
    pub object_id: Option<Uuid>,
    pub request_id: Option<String>,
    /// The trace of the request, so the entry can open it; none outside a request.
    pub trace_id: Option<Uuid>,
    pub summary: Value,
}

#[derive(Clone, Debug, FromRow)]
pub struct AuditEntry {
    pub id: i64,
    pub workspace_id: Option<Uuid>,
    pub actor_kind: String,
    pub actor_id: Option<Uuid>,
    pub action: String,
    pub object_kind: Option<String>,
    pub object_id: Option<Uuid>,
    pub request_id: Option<String>,
    pub trace_id: Option<Uuid>,
    pub summary: Value,
    pub occurred_at: OffsetDateTime,
}

pub async fn append_audit<'e>(
    executor: impl PgExecutor<'e>,
    entry: &NewAuditEntry,
) -> Result<(), StoreError> {
    debug_assert!(
        audit::is_known(entry.action),
        "audit action {:?} is not in zirv_flow_core::audit::ACTIONS",
        entry.action
    );
    sqlx::query(
        "INSERT INTO audit_log \
            (workspace_id, actor_kind, actor_id, action, object_kind, object_id, request_id, \
             trace_id, summary) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(entry.workspace_id.map(|id| id.as_uuid()))
    .bind(entry.actor.kind.as_str())
    .bind(entry.actor.id)
    .bind(entry.action)
    .bind(entry.object_kind)
    .bind(entry.object_id)
    .bind(entry.request_id.as_deref())
    .bind(entry.trace_id)
    .bind(&entry.summary)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// The newest rows first, at most `limit`.
pub async fn list_audit<'e>(
    executor: impl PgExecutor<'e>,
    limit: i64,
) -> Result<Vec<AuditEntry>, StoreError> {
    sqlx::query_as(
        "SELECT id, workspace_id, actor_kind, actor_id, action, object_kind, object_id, \
                request_id, trace_id, summary, occurred_at \
         FROM audit_log ORDER BY id DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// The rows a read selects. `workspace: None` selects the instance rows (those with no
/// workspace); a workspace read never sees them.
#[derive(Clone, Debug, Default)]
pub struct AuditFilter {
    pub workspace: Option<WorkspaceId>,
    /// An exact action, or an area prefix ending in a dot such as `flow.`.
    pub action: Option<String>,
    pub actor_kind: Option<String>,
    pub actor_id: Option<Uuid>,
    pub object_kind: Option<String>,
    pub object_id: Option<Uuid>,
    /// Inclusive.
    pub from: Option<OffsetDateTime>,
    /// Exclusive.
    pub to: Option<OffsetDateTime>,
    pub request_id: Option<String>,
}

/// One page, newest first.
#[derive(Clone, Debug)]
pub struct AuditPage {
    pub entries: Vec<AuditEntry>,
    /// Pass as the cursor for the next page; none when there is none.
    pub next: Option<i64>,
}

/// The rows an export reads per query: the most it holds in memory at once.
pub const AUDIT_BATCH: i64 = 500;

const COLUMNS: &str = "id, workspace_id, actor_kind, actor_id, action, object_kind, object_id, \
                       request_id, trace_id, summary, occurred_at";

fn select(head: &str, filter: &AuditFilter, before: Option<i64>) -> QueryBuilder<Postgres> {
    let mut query = QueryBuilder::new(head);
    query.push(" FROM audit_log WHERE ");
    match filter.workspace {
        Some(workspace) => {
            query.push("workspace_id = ").push_bind(workspace.as_uuid());
        }
        None => {
            query.push("workspace_id IS NULL");
        }
    }
    if let Some(action) = &filter.action {
        if action.ends_with('.') {
            query
                .push(" AND starts_with(action, ")
                .push_bind(action.clone())
                .push(")");
        } else {
            query.push(" AND action = ").push_bind(action.clone());
        }
    }
    if let Some(kind) = &filter.actor_kind {
        query.push(" AND actor_kind = ").push_bind(kind.clone());
    }
    if let Some(actor) = filter.actor_id {
        query.push(" AND actor_id = ").push_bind(actor);
    }
    if let Some(kind) = &filter.object_kind {
        query.push(" AND object_kind = ").push_bind(kind.clone());
    }
    if let Some(object) = filter.object_id {
        query.push(" AND object_id = ").push_bind(object);
    }
    if let Some(from) = filter.from {
        query.push(" AND occurred_at >= ").push_bind(from);
    }
    if let Some(to) = filter.to {
        query.push(" AND occurred_at < ").push_bind(to);
    }
    if let Some(request) = &filter.request_id {
        query.push(" AND request_id = ").push_bind(request.clone());
    }
    if let Some(before) = before {
        query.push(" AND id < ").push_bind(before);
    }
    query
}

/// Up to `limit` rows matching `filter`, newest first, starting below the row id `cursor`. Rows
/// appended while a caller pages have higher ids than any cursor, so they never shift a page.
///
/// Ids are not commit-ordered: a transaction that took a lower id and commits after a walk has
/// read past it is missing from that walk. An entry committed during a walk may be absent.
pub async fn list_audit_page<'e>(
    executor: impl PgExecutor<'e>,
    filter: &AuditFilter,
    cursor: Option<i64>,
    limit: i64,
) -> Result<AuditPage, StoreError> {
    let mut query = select(&format!("SELECT {COLUMNS}"), filter, cursor);
    query
        .push(" ORDER BY id DESC LIMIT ")
        .push_bind(limit.saturating_add(1));
    let mut entries: Vec<AuditEntry> = query
        .build_query_as()
        .fetch_all(executor)
        .await
        .map_err(StoreError::query)?;
    let limit = usize::try_from(limit).unwrap_or(0);
    let more = entries.len() > limit;
    entries.truncate(limit);
    let next = entries.last().map(|last| last.id).filter(|_| more);
    Ok(AuditPage { entries, next })
}

/// How many rows match `filter`, and the highest id among them.
pub async fn count_audit<'e>(
    executor: impl PgExecutor<'e>,
    filter: &AuditFilter,
) -> Result<(i64, Option<i64>), StoreError> {
    select("SELECT count(*), max(id)", filter, None)
        .build_query_as()
        .fetch_one(executor)
        .await
        .map_err(StoreError::query)
}

/// The rows matching `filter` with an id up to `through`, newest first, as batches of at most
/// [`AUDIT_BATCH`] rows read by keyset. One batch is in memory at a time, so an export of any
/// size streams. Like [`list_audit_page`], the stream can miss an entry whose transaction took a
/// lower id and committed after the stream read past it.
pub fn stream_audit(
    pool: PgPool,
    filter: AuditFilter,
    through: i64,
) -> impl Stream<Item = Result<Vec<AuditEntry>, StoreError>> {
    try_unfold(Some(through.saturating_add(1)), move |cursor| {
        let pool = pool.clone();
        let filter = filter.clone();
        async move {
            let Some(cursor) = cursor else {
                return Ok(None);
            };
            let page = list_audit_page(&pool, &filter, Some(cursor), AUDIT_BATCH).await?;
            if page.entries.is_empty() {
                return Ok(None);
            }
            Ok(Some((page.entries, page.next)))
        }
    })
}

/// Deletes up to `batch` of the oldest rows before `cutoff` through `audit_purge`, the only
/// deleter the database allows. Returns how many it deleted. `batch` is 1 to 10000: the function
/// refuses anything else and deletes nothing. Only the owning role may execute `audit_purge`.
pub async fn purge_audit<'e>(
    executor: impl PgExecutor<'e>,
    cutoff: OffsetDateTime,
    batch: i32,
) -> Result<i64, StoreError> {
    sqlx::query_scalar("SELECT audit_purge($1, $2)")
        .bind(cutoff)
        .bind(batch)
        .fetch_one(executor)
        .await
        .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use futures_util::TryStreamExt;
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration;

    use super::*;

    fn entry(workspace: Option<WorkspaceId>, action: &'static str) -> NewAuditEntry {
        NewAuditEntry {
            workspace_id: workspace,
            actor: Actor::system(),
            action,
            object_kind: None,
            object_id: None,
            request_id: None,
            trace_id: None,
            summary: json!({}),
        }
    }

    async fn page(pool: &PgPool, filter: &AuditFilter) -> Vec<String> {
        let page = list_audit_page(pool, filter, None, 100).await.unwrap();
        page.entries.into_iter().map(|e| e.action).collect()
    }

    #[test]
    fn actors_carry_their_kind_and_id() {
        let id = Uuid::new_v4();

        assert_eq!(Actor::user(id).kind.as_str(), "user");
        assert_eq!(Actor::api_key(id).kind.as_str(), "api_key");
        assert_eq!(Actor::system().id, None);
        assert_eq!(Actor::anonymous().kind.as_str(), "anonymous");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn appended_rows_are_listed_newest_first(pool: PgPool) {
        let user = Uuid::new_v4();
        for action in ["flow.created", "flow.paused"] {
            append_audit(
                &pool,
                &NewAuditEntry {
                    workspace_id: None,
                    actor: Actor::user(user),
                    action,
                    object_kind: Some("thing"),
                    object_id: None,
                    request_id: Some("req-1".into()),
                    trace_id: None,
                    summary: json!({"reason": "test"}),
                },
            )
            .await
            .unwrap();
        }

        let rows = list_audit(&pool, 10).await.unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].action, "flow.paused");
        assert_eq!(rows[0].actor_kind, "user");
        assert_eq!(rows[0].actor_id, Some(user));
        assert_eq!(rows[0].request_id.as_deref(), Some("req-1"));
        assert_eq!(rows[0].summary, json!({"reason": "test"}));
        assert_eq!(list_audit(&pool, 1).await.unwrap().len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_unknown_actor_kind_is_refused_by_the_database(pool: PgPool) {
        let result =
            sqlx::query("INSERT INTO audit_log (actor_kind, action) VALUES ('stranger', 'x.y')")
                .execute(&pool)
                .await;

        assert!(result.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn each_filter_narrows_the_page(pool: PgPool) {
        let ws = WorkspaceId::new();
        let (actor, object, trace) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let mut flow = entry(Some(ws), "flow.created");
        flow.actor = Actor::user(actor);
        flow.object_kind = Some("flow");
        flow.object_id = Some(object);
        flow.request_id = Some("req-a".into());
        flow.trace_id = Some(trace);
        append_audit(&pool, &flow).await.unwrap();
        append_audit(&pool, &entry(Some(ws), "flow.paused"))
            .await
            .unwrap();
        append_audit(&pool, &entry(Some(ws), "list.created"))
            .await
            .unwrap();
        let base = AuditFilter {
            workspace: Some(ws),
            ..AuditFilter::default()
        };

        let by = |filter: AuditFilter| {
            let pool = pool.clone();
            async move { page(&pool, &filter).await }
        };
        assert_eq!(by(base.clone()).await.len(), 3);
        let exact = AuditFilter {
            action: Some("flow.paused".into()),
            ..base.clone()
        };
        assert_eq!(by(exact).await, ["flow.paused"]);
        let area = AuditFilter {
            action: Some("flow.".into()),
            ..base.clone()
        };
        assert_eq!(by(area).await, ["flow.paused", "flow.created"]);
        let kind = AuditFilter {
            actor_kind: Some("user".into()),
            ..base.clone()
        };
        assert_eq!(by(kind).await, ["flow.created"]);
        let who = AuditFilter {
            actor_id: Some(actor),
            ..base.clone()
        };
        assert_eq!(by(who).await, ["flow.created"]);
        let object_kind = AuditFilter {
            object_kind: Some("flow".into()),
            ..base.clone()
        };
        assert_eq!(by(object_kind).await, ["flow.created"]);
        let what = AuditFilter {
            object_id: Some(object),
            ..base.clone()
        };
        assert_eq!(by(what).await, ["flow.created"]);
        let request = AuditFilter {
            request_id: Some("req-a".into()),
            ..base.clone()
        };
        assert_eq!(by(request).await, ["flow.created"]);
        let now = OffsetDateTime::now_utc();
        let later = AuditFilter {
            from: Some(now + Duration::hours(1)),
            ..base.clone()
        };
        assert!(by(later).await.is_empty());
        let earlier = AuditFilter {
            to: Some(now - Duration::hours(1)),
            ..base.clone()
        };
        assert!(by(earlier).await.is_empty());
        let within = AuditFilter {
            from: Some(now - Duration::hours(1)),
            to: Some(now + Duration::hours(1)),
            ..base.clone()
        };
        assert_eq!(by(within).await.len(), 3);
        let first = list_audit_page(&pool, &base, None, 10).await.unwrap();
        assert_eq!(first.entries[2].trace_id, Some(trace));
        assert_eq!(first.entries[0].trace_id, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_cursor_round_trips_and_appends_do_not_shift_pages(pool: PgPool) {
        let ws = WorkspaceId::new();
        for _ in 0..5 {
            append_audit(&pool, &entry(Some(ws), "flow.created"))
                .await
                .unwrap();
        }
        let filter = AuditFilter {
            workspace: Some(ws),
            ..AuditFilter::default()
        };

        let first = list_audit_page(&pool, &filter, None, 2).await.unwrap();
        append_audit(&pool, &entry(Some(ws), "flow.paused"))
            .await
            .unwrap();
        append_audit(&pool, &entry(Some(ws), "flow.paused"))
            .await
            .unwrap();
        let second = list_audit_page(&pool, &filter, first.next, 2)
            .await
            .unwrap();
        let third = list_audit_page(&pool, &filter, second.next, 2)
            .await
            .unwrap();

        let ids: Vec<i64> = [&first, &second, &third]
            .iter()
            .flat_map(|p| p.entries.iter().map(|e| e.id))
            .collect();
        assert_eq!(ids.len(), 5);
        assert!(ids.windows(2).all(|pair| pair[0] > pair[1]));
        assert!(first.next.is_some() && second.next.is_some() && third.next.is_none());
        assert!(third.entries.iter().all(|e| e.action == "flow.created"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn instance_rows_never_appear_in_a_workspace_page(pool: PgPool) {
        let ws = WorkspaceId::new();
        append_audit(&pool, &entry(None, "session.signed_in"))
            .await
            .unwrap();
        append_audit(&pool, &entry(Some(ws), "flow.created"))
            .await
            .unwrap();
        append_audit(&pool, &entry(Some(WorkspaceId::new()), "list.created"))
            .await
            .unwrap();

        let workspace = AuditFilter {
            workspace: Some(ws),
            ..AuditFilter::default()
        };
        let instance = AuditFilter::default();

        assert_eq!(page(&pool, &workspace).await, ["flow.created"]);
        assert_eq!(page(&pool, &instance).await, ["session.signed_in"]);
        assert_eq!(count_audit(&pool, &instance).await.unwrap().0, 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn update_delete_and_truncate_are_refused(pool: PgPool) {
        append_audit(&pool, &entry(None, "session.signed_in"))
            .await
            .unwrap();

        for sql in [
            "UPDATE audit_log SET summary = '{}'",
            "DELETE FROM audit_log",
            "TRUNCATE audit_log",
        ] {
            let error = sqlx::query(sql).execute(&pool).await.unwrap_err();
            assert!(error.to_string().contains("append-only"), "{sql}: {error}");
        }

        assert_eq!(list_audit(&pool, 10).await.unwrap().len(), 1);
        append_audit(&pool, &entry(None, "session.signed_out"))
            .await
            .unwrap();
        assert_eq!(list_audit(&pool, 10).await.unwrap().len(), 2);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn audit_purge_deletes_only_rows_before_the_cutoff_in_batches(pool: PgPool) {
        for age_days in [10, 9, 8, 1] {
            sqlx::query(
                "INSERT INTO audit_log (actor_kind, action, occurred_at) \
                 VALUES ('system', 'session.signed_in', now() - make_interval(days => $1))",
            )
            .bind(age_days)
            .execute(&pool)
            .await
            .unwrap();
        }
        let cutoff = OffsetDateTime::now_utc() - Duration::days(5);

        assert_eq!(purge_audit(&pool, cutoff, 2).await.unwrap(), 2);
        assert_eq!(purge_audit(&pool, cutoff, 2).await.unwrap(), 1);
        assert_eq!(purge_audit(&pool, cutoff, 2).await.unwrap(), 0);
        assert_eq!(list_audit(&pool, 10).await.unwrap().len(), 1);
        assert!(purge_audit(&pool, cutoff, 0).await.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_purge_batch_above_the_limit_is_refused_and_deletes_nothing(pool: PgPool) {
        sqlx::query(
            "INSERT INTO audit_log (actor_kind, action, occurred_at) \
             VALUES ('system', 'session.signed_in', now() - interval '10 days')",
        )
        .execute(&pool)
        .await
        .unwrap();
        let cutoff = OffsetDateTime::now_utc() - Duration::days(5);

        assert!(purge_audit(&pool, cutoff, 10_001).await.is_err());

        assert_eq!(list_audit(&pool, 10).await.unwrap().len(), 1);
        assert_eq!(purge_audit(&pool, cutoff, 10_000).await.unwrap(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn audit_purge_is_not_executable_by_public(pool: PgPool) {
        let public: bool = sqlx::query_scalar(
            "SELECT has_function_privilege('public', 'audit_purge(timestamptz,int)', 'EXECUTE')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert!(!public);
        assert_eq!(
            purge_audit(&pool, OffsetDateTime::now_utc(), 1)
                .await
                .unwrap(),
            0,
            "the owner keeps EXECUTE"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_purge_permission_ends_with_the_purge(pool: PgPool) {
        append_audit(&pool, &entry(None, "session.signed_in"))
            .await
            .unwrap();
        let mut tx = pool.begin().await.unwrap();

        purge_audit(&mut *tx, OffsetDateTime::now_utc() - Duration::days(1), 10)
            .await
            .unwrap();
        let after = sqlx::query("DELETE FROM audit_log").execute(&mut *tx).await;

        assert!(
            after.is_err(),
            "a later statement must not inherit the permission"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_stream_reads_in_bounded_batches_up_to_the_snapshot(pool: PgPool) {
        let ws = WorkspaceId::new();
        sqlx::query(
            "INSERT INTO audit_log (workspace_id, actor_kind, action) \
             SELECT $1, 'system', 'flow.created' FROM generate_series(1, 100000)",
        )
        .bind(ws.as_uuid())
        .execute(&pool)
        .await
        .unwrap();
        let filter = AuditFilter {
            workspace: Some(ws),
            ..AuditFilter::default()
        };
        let (count, through) = count_audit(&pool, &filter).await.unwrap();
        append_audit(&pool, &entry(Some(ws), "flow.paused"))
            .await
            .unwrap();

        let batches: Vec<Vec<AuditEntry>> = stream_audit(pool.clone(), filter, through.unwrap())
            .try_collect()
            .await
            .unwrap();

        assert_eq!(count, 100_000);
        assert_eq!(batches.len(), 200);
        assert!(batches.iter().all(|b| b.len() <= AUDIT_BATCH as usize));
        assert_eq!(batches.iter().map(Vec::len).sum::<usize>(), 100_000);
        assert!(batches.iter().flatten().all(|e| e.action == "flow.created"));
    }
}
