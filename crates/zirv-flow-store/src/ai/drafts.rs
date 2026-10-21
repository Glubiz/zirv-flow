//! Assistant drafts (architecture 13.2): a mapping or manifest the model drafted and the validator
//! approved, kept until its author accepts it into an editor or it expires. A draft that never
//! validated is stored without content, so raw model output is never kept. A draft belongs to the
//! user who ran the assistant, and every read and write is scoped to that user and workspace.

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};

use crate::StoreError;

/// The most drafts one purge deletes; the job repeats until a pass finds fewer.
pub const DRAFT_PURGE_BATCH: i64 = 1000;

/// What a draft is a draft of.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DraftKind {
    Mapping,
    Manifest,
    /// A segment definition. It names a segment (or none, for a new one) and no source.
    Segment,
    /// A flow graph with its settings. It names the flow whose draft it would replace.
    Flow,
}

impl DraftKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mapping => "mapping",
            Self::Manifest => "manifest",
            Self::Segment => "segment",
            Self::Flow => "flow",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "mapping" => Some(Self::Mapping),
            "manifest" => Some(Self::Manifest),
            "segment" => Some(Self::Segment),
            "flow" => Some(Self::Flow),
            _ => None,
        }
    }
}

/// Where a draft stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DraftStatus {
    /// Approved by the validator; it can be accepted.
    Valid,
    /// No attempt validated; the draft has no content and cannot be accepted.
    Invalid,
    /// Its author loaded it into an editor.
    Accepted,
}

impl DraftStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Invalid => "invalid",
            Self::Accepted => "accepted",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "valid" => Some(Self::Valid),
            "invalid" => Some(Self::Invalid),
            "accepted" => Some(Self::Accepted),
            _ => None,
        }
    }
}

/// A draft to store. `content` is `None` exactly when `status` is `Invalid`.
#[derive(Clone, Debug)]
pub struct NewAiDraft<'a> {
    pub id: Uuid,
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    pub kind: DraftKind,
    pub source_id: Option<Uuid>,
    pub stream_name: Option<&'a str>,
    /// The segment a segment draft would change; none for a new segment and for other kinds.
    pub segment_id: Option<Uuid>,
    /// The flow a flow draft would change; none for the other kinds.
    pub flow_id: Option<Uuid>,
    pub status: DraftStatus,
    pub content: Option<&'a Value>,
    /// SHA-256 of the saved artifact the draft was compared with.
    pub base_digest: Option<[u8; 32]>,
    pub expires_at: OffsetDateTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AiDraft {
    pub id: Uuid,
    pub workspace_id: WorkspaceId,
    pub user_id: UserId,
    pub kind: DraftKind,
    pub source_id: Option<Uuid>,
    pub stream_name: Option<String>,
    pub segment_id: Option<Uuid>,
    pub flow_id: Option<Uuid>,
    pub status: DraftStatus,
    pub content: Option<Value>,
    pub base_digest: Option<[u8; 32]>,
    pub created_at: OffsetDateTime,
    pub accepted_at: Option<OffsetDateTime>,
    pub expires_at: OffsetDateTime,
}

#[derive(FromRow)]
struct Row {
    id: Uuid,
    workspace_id: Uuid,
    user_id: Uuid,
    kind: String,
    source_id: Option<Uuid>,
    stream_name: Option<String>,
    segment_id: Option<Uuid>,
    flow_id: Option<Uuid>,
    status: String,
    content: Option<Value>,
    base_digest: Option<Vec<u8>>,
    created_at: OffsetDateTime,
    accepted_at: Option<OffsetDateTime>,
    expires_at: OffsetDateTime,
}

impl TryFrom<Row> for AiDraft {
    type Error = StoreError;

    fn try_from(row: Row) -> Result<Self, StoreError> {
        let corrupt = || StoreError::query(sqlx::Error::RowNotFound);
        Ok(Self {
            id: row.id,
            workspace_id: WorkspaceId::from_uuid(row.workspace_id),
            user_id: UserId::from_uuid(row.user_id),
            kind: DraftKind::parse(&row.kind).ok_or_else(corrupt)?,
            source_id: row.source_id,
            stream_name: row.stream_name,
            segment_id: row.segment_id,
            flow_id: row.flow_id,
            status: DraftStatus::parse(&row.status).ok_or_else(corrupt)?,
            content: row.content,
            base_digest: row
                .base_digest
                .map(|bytes| <[u8; 32]>::try_from(bytes).map_err(|_| corrupt()))
                .transpose()?,
            created_at: row.created_at,
            accepted_at: row.accepted_at,
            expires_at: row.expires_at,
        })
    }
}

const COLUMNS: &str = "id, workspace_id, user_id, kind, source_id, stream_name, \
    segment_id, flow_id, status, content, base_digest, created_at, accepted_at, expires_at";

/// Stores a draft. The table's checks refuse an invalid draft with content, a valid one without,
/// and a mapping without its source and stream.
pub async fn insert_ai_draft(
    connection: &mut PgConnection,
    new: &NewAiDraft<'_>,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO ai_drafts (id, workspace_id, user_id, kind, source_id, stream_name, \
            segment_id, flow_id, status, content, base_digest, expires_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(new.id)
    .bind(new.workspace_id.as_uuid())
    .bind(new.user_id.as_uuid())
    .bind(new.kind.as_str())
    .bind(new.source_id)
    .bind(new.stream_name)
    .bind(new.segment_id)
    .bind(new.flow_id)
    .bind(new.status.as_str())
    .bind(new.content)
    .bind(new.base_digest.as_ref().map(<[u8; 32]>::as_slice))
    .bind(new.expires_at)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// The user's own draft in the workspace, when it has not expired at `now`.
pub async fn find_ai_draft<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    user: UserId,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<Option<AiDraft>, StoreError> {
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM ai_drafts \
         WHERE id = $1 AND workspace_id = $2 AND user_id = $3 AND expires_at > $4"
    )))
    .bind(id)
    .bind(workspace.as_uuid())
    .bind(user.as_uuid())
    .bind(now)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    row.map(AiDraft::try_from).transpose()
}

/// Marks the user's own unexpired draft accepted and returns whether it was found. A `valid` or
/// already `accepted` draft becomes `accepted` (the first acceptance time is kept); an `invalid`
/// draft is never accepted.
pub async fn mark_ai_draft_accepted(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    user: UserId,
    id: Uuid,
    now: OffsetDateTime,
) -> Result<bool, StoreError> {
    let done = sqlx::query(
        "UPDATE ai_drafts SET status = 'accepted', accepted_at = COALESCE(accepted_at, $4) \
         WHERE id = $1 AND workspace_id = $2 AND user_id = $3 AND expires_at > $4 \
           AND status IN ('valid', 'accepted')",
    )
    .bind(id)
    .bind(workspace.as_uuid())
    .bind(user.as_uuid())
    .bind(now)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected() > 0)
}

/// Deletes up to `limit` drafts that expired at or before `now`, across all workspaces, and
/// returns how many. Rows another purge holds are skipped, so concurrent runs do not collide.
pub async fn purge_expired_ai_drafts(
    connection: &mut PgConnection,
    now: OffsetDateTime,
    limit: i64,
) -> Result<u64, StoreError> {
    let done = sqlx::query(
        "DELETE FROM ai_drafts WHERE id IN ( \
            SELECT id FROM ai_drafts WHERE expires_at <= $1 \
            ORDER BY expires_at LIMIT $2 FOR UPDATE SKIP LOCKED)",
    )
    .bind(now)
    .bind(limit)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use time::Duration;
    use time::macros::datetime;
    use zirv_flow_core::{Email, Slug, WorkspaceName};

    use super::*;
    use crate::{
        NewSegment, NewSegmentVersion, NewSource, create_segment, create_source, create_user,
        create_workspace, delete_source, lock_segments,
    };

    async fn workspace(pool: &PgPool, name: &str) -> WorkspaceId {
        let name_parsed = WorkspaceName::parse(name).unwrap();
        let slug = Slug::parse(name).unwrap();
        create_workspace(pool, &name_parsed, &slug)
            .await
            .unwrap()
            .id
    }

    async fn user(pool: &PgPool, email: &str) -> UserId {
        create_user(pool, &Email::parse(email).unwrap(), None, false)
            .await
            .unwrap()
            .id
    }

    async fn source(pool: &PgPool, ws: WorkspaceId) -> Uuid {
        let config = json!({});
        let id = Uuid::now_v7();
        create_source(
            pool,
            ws,
            &NewSource {
                id,
                kind: "fake",
                name: "crm",
                config: &config,
                credential_secret_id: None,
                credential_fields: &[],
                schedule: None,
                created_by: None,
            },
        )
        .await
        .unwrap();
        id
    }

    fn mapping<'a>(
        ws: WorkspaceId,
        user: UserId,
        source_id: Uuid,
        content: Option<&'a Value>,
        expires_at: OffsetDateTime,
    ) -> NewAiDraft<'a> {
        NewAiDraft {
            id: Uuid::now_v7(),
            workspace_id: ws,
            user_id: user,
            kind: DraftKind::Mapping,
            source_id: Some(source_id),
            stream_name: Some("contacts"),
            segment_id: None,
            flow_id: None,
            status: if content.is_some() {
                DraftStatus::Valid
            } else {
                DraftStatus::Invalid
            },
            content,
            base_digest: Some([7; 32]),
            expires_at,
        }
    }

    async fn insert(pool: &PgPool, draft: &NewAiDraft<'_>) {
        let mut connection = pool.acquire().await.unwrap();
        insert_ai_draft(&mut connection, draft).await.unwrap();
    }

    #[test]
    fn kinds_and_statuses_round_trip_through_text() {
        for kind in [DraftKind::Mapping, DraftKind::Manifest, DraftKind::Segment] {
            assert_eq!(DraftKind::parse(kind.as_str()), Some(kind));
        }
        for status in [
            DraftStatus::Valid,
            DraftStatus::Invalid,
            DraftStatus::Accepted,
        ] {
            assert_eq!(DraftStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(DraftKind::parse("flow"), None);
        assert_eq!(DraftStatus::parse("published"), None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_draft_round_trips_for_its_owner_only(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let (ada, bob) = (
            user(&pool, "ada@example.com").await,
            user(&pool, "bob@example.com").await,
        );
        let source_id = source(&pool, one).await;
        let now = datetime!(2026-05-15 12:00 UTC);
        let content = json!({"columns": []});
        let draft = mapping(one, ada, source_id, Some(&content), now + Duration::days(7));
        insert(&pool, &draft).await;

        let found = find_ai_draft(&pool, one, ada, draft.id, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.content.as_ref(), Some(&content));
        assert_eq!(found.status, DraftStatus::Valid);
        assert_eq!(found.base_digest, Some([7; 32]));
        assert_eq!(found.stream_name.as_deref(), Some("contacts"));
        // Another user, another workspace and an expired draft are all absent.
        assert!(
            find_ai_draft(&pool, one, bob, draft.id, now)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_ai_draft(&pool, two, ada, draft.id, now)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_ai_draft(&pool, one, ada, draft.id, now + Duration::days(7))
                .await
                .unwrap()
                .is_none()
        );
        // Only the owner can accept it, and accepting twice keeps the first time.
        let mut connection = pool.acquire().await.unwrap();
        assert!(
            !mark_ai_draft_accepted(&mut connection, one, bob, draft.id, now)
                .await
                .unwrap()
        );
        assert!(
            mark_ai_draft_accepted(&mut connection, one, ada, draft.id, now)
                .await
                .unwrap()
        );
        assert!(
            mark_ai_draft_accepted(
                &mut connection,
                one,
                ada,
                draft.id,
                now + Duration::hours(1)
            )
            .await
            .unwrap()
        );
        let accepted = find_ai_draft(&pool, one, ada, draft.id, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(accepted.status, DraftStatus::Accepted);
        assert_eq!(accepted.accepted_at, Some(now));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_invalid_draft_is_never_marked_accepted(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let ada = user(&pool, "ada@example.com").await;
        let source_id = source(&pool, one).await;
        let now = datetime!(2026-05-15 12:00 UTC);
        let draft = mapping(one, ada, source_id, None, now + Duration::days(7));
        insert(&pool, &draft).await;
        let mut connection = pool.acquire().await.unwrap();

        let accepted = mark_ai_draft_accepted(&mut connection, one, ada, draft.id, now)
            .await
            .unwrap();

        assert!(!accepted);
        let found = find_ai_draft(&pool, one, ada, draft.id, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((found.status, found.content), (DraftStatus::Invalid, None));
        // The table refuses the two inconsistent shapes outright.
        let content = json!({});
        let mut with_content = mapping(one, ada, source_id, Some(&content), now);
        with_content.status = DraftStatus::Invalid;
        assert!(
            insert_ai_draft(&mut connection, &with_content)
                .await
                .is_err()
        );
        let mut without = mapping(one, ada, source_id, None, now);
        without.status = DraftStatus::Valid;
        assert!(insert_ai_draft(&mut connection, &without).await.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn expired_drafts_are_purged_in_batches(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let ada = user(&pool, "ada@example.com").await;
        let source_id = source(&pool, one).await;
        let now = datetime!(2026-05-15 12:00 UTC);
        let content = json!({});
        for days in [-2, -1, 0] {
            let draft = mapping(
                one,
                ada,
                source_id,
                Some(&content),
                now + Duration::days(days),
            );
            insert(&pool, &draft).await;
        }
        let live = mapping(one, ada, source_id, Some(&content), now + Duration::days(1));
        insert(&pool, &live).await;
        let mut connection = pool.acquire().await.unwrap();

        let first = purge_expired_ai_drafts(&mut connection, now, 2)
            .await
            .unwrap();
        let second = purge_expired_ai_drafts(&mut connection, now, 2)
            .await
            .unwrap();
        let third = purge_expired_ai_drafts(&mut connection, now, 2)
            .await
            .unwrap();

        assert_eq!((first, second, third), (2, 1, 0));
        assert!(
            find_ai_draft(&pool, one, ada, live.id, now)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_draft_goes_with_its_source_and_user(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let (ada, bob) = (
            user(&pool, "ada@example.com").await,
            user(&pool, "bob@example.com").await,
        );
        let (first, second) = (source(&pool, one).await, {
            let config = json!({});
            let id = Uuid::now_v7();
            create_source(
                &pool,
                one,
                &NewSource {
                    id,
                    kind: "fake",
                    name: "other",
                    config: &config,
                    credential_secret_id: None,
                    credential_fields: &[],
                    schedule: None,
                    created_by: None,
                },
            )
            .await
            .unwrap();
            id
        });
        let now = datetime!(2026-05-15 12:00 UTC);
        let content = json!({});
        let later = now + Duration::days(7);
        insert(&pool, &mapping(one, ada, first, Some(&content), later)).await;
        insert(&pool, &mapping(one, bob, second, Some(&content), later)).await;
        let count = || async {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM ai_drafts")
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        assert_eq!(count().await, 2);

        let mut connection = pool.acquire().await.unwrap();
        delete_source(&mut connection, one, first).await.unwrap();
        assert_eq!(count().await, 1);

        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(bob.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(count().await, 0);
    }

    async fn segment(pool: &PgPool, ws: WorkspaceId, name: &str) -> Uuid {
        let mut tx = crate::begin(pool).await.unwrap();
        lock_segments(&mut tx, ws).await.unwrap();
        let new = NewSegment {
            id: Uuid::now_v7(),
            name: name.into(),
            description: String::new(),
            cadence_minutes: 60,
        };
        let version = NewSegmentVersion {
            definition: json!({"v": 1}),
            normalized: json!({"v": 1}),
            deps: json!({"segments": []}),
            time_sensitive: false,
        };
        let trace = zirv_flow_core::TraceContext::root(zirv_flow_core::Cause::api("t"));
        let (segment, _) = create_segment(&mut tx, ws, &new, &version, None, &trace)
            .await
            .unwrap();
        crate::commit(tx).await.unwrap();
        segment.id
    }

    fn segment_draft<'a>(
        ws: WorkspaceId,
        user: UserId,
        segment_id: Option<Uuid>,
        content: &'a Value,
        expires_at: OffsetDateTime,
    ) -> NewAiDraft<'a> {
        NewAiDraft {
            id: Uuid::now_v7(),
            workspace_id: ws,
            user_id: user,
            kind: DraftKind::Segment,
            source_id: None,
            stream_name: None,
            segment_id,
            flow_id: None,
            status: DraftStatus::Valid,
            content: Some(content),
            base_digest: Some([3; 32]),
            expires_at,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_segment_draft_is_stored_for_a_segment_or_a_new_one(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let ada = user(&pool, "ada@example.com").await;
        let saved = segment(&pool, one, "Buyers").await;
        let now = datetime!(2026-05-15 12:00 UTC);
        let content = json!({"v": 1, "trait": "plan", "op": "exists"});
        let later = now + Duration::days(7);
        let existing = segment_draft(one, ada, Some(saved), &content, later);
        let new = segment_draft(one, ada, None, &content, later);
        insert(&pool, &existing).await;
        insert(&pool, &new).await;

        let found = find_ai_draft(&pool, one, ada, existing.id, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.kind, DraftKind::Segment);
        assert_eq!(found.segment_id, Some(saved));
        assert_eq!((found.source_id, found.stream_name), (None, None));
        assert_eq!(found.content.as_ref(), Some(&content));
        let fresh = find_ai_draft(&pool, one, ada, new.id, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fresh.segment_id, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_kind_rules_keep_mapping_and_manifest_drafts_valid_and_refuse_mixed_targets(
        pool: PgPool,
    ) {
        let one = workspace(&pool, "one").await;
        let ada = user(&pool, "ada@example.com").await;
        let source_id = source(&pool, one).await;
        let saved = segment(&pool, one, "Buyers").await;
        let now = datetime!(2026-05-15 12:00 UTC);
        let later = now + Duration::days(7);
        let content = json!({});
        let mut connection = pool.acquire().await.unwrap();

        // The two older kinds keep their rules.
        insert_ai_draft(
            &mut connection,
            &mapping(one, ada, source_id, Some(&content), later),
        )
        .await
        .unwrap();
        let manifest = NewAiDraft {
            kind: DraftKind::Manifest,
            stream_name: None,
            ..mapping(one, ada, source_id, Some(&content), later)
        };
        insert_ai_draft(&mut connection, &manifest).await.unwrap();
        let unsourced_mapping = NewAiDraft {
            source_id: None,
            ..mapping(one, ada, source_id, Some(&content), later)
        };
        assert!(
            insert_ai_draft(&mut connection, &unsourced_mapping)
                .await
                .is_err()
        );
        let streamed_manifest = NewAiDraft {
            kind: DraftKind::Manifest,
            ..mapping(one, ada, source_id, Some(&content), later)
        };
        assert!(
            insert_ai_draft(&mut connection, &streamed_manifest)
                .await
                .is_err()
        );
        // Only a segment draft names a segment, and it names no source or stream.
        let mapped_segment = NewAiDraft {
            segment_id: Some(saved),
            ..mapping(one, ada, source_id, Some(&content), later)
        };
        assert!(
            insert_ai_draft(&mut connection, &mapped_segment)
                .await
                .is_err()
        );
        let sourced_segment = NewAiDraft {
            source_id: Some(source_id),
            ..segment_draft(one, ada, None, &content, later)
        };
        assert!(
            insert_ai_draft(&mut connection, &sourced_segment)
                .await
                .is_err()
        );
        let streamed_segment = NewAiDraft {
            stream_name: Some("contacts"),
            ..segment_draft(one, ada, None, &content, later)
        };
        assert!(
            insert_ai_draft(&mut connection, &streamed_segment)
                .await
                .is_err()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_segment_draft_names_a_segment_of_its_own_workspace(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let ada = user(&pool, "ada@example.com").await;
        let foreign = segment(&pool, two, "Elsewhere").await;
        let now = datetime!(2026-05-15 12:00 UTC);
        let content = json!({});
        let mut connection = pool.acquire().await.unwrap();

        let refused = insert_ai_draft(
            &mut connection,
            &segment_draft(one, ada, Some(foreign), &content, now + Duration::days(7)),
        )
        .await;

        assert!(refused.is_err());
    }
    async fn flow(pool: &PgPool, ws: WorkspaceId, name: &str) -> Uuid {
        let mut tx = crate::begin(pool).await.unwrap();
        let new = crate::NewFlow {
            id: Uuid::now_v7(),
            name: name.to_owned(),
            graph: json!({"v": 1, "nodes": [], "edges": []}),
            settings: json!({"trigger": {"type": "manual"}, "reentry": {"mode": "once"}}),
        };
        let made = crate::create_flow(&mut tx, ws, &new, None).await.unwrap();
        crate::commit(tx).await.unwrap();
        made.id
    }

    fn flow_draft<'a>(
        ws: WorkspaceId,
        user: UserId,
        flow_id: Option<Uuid>,
        content: &'a Value,
        expires_at: OffsetDateTime,
    ) -> NewAiDraft<'a> {
        NewAiDraft {
            kind: DraftKind::Flow,
            flow_id,
            source_id: None,
            stream_name: None,
            segment_id: None,
            ..segment_draft(ws, user, None, content, expires_at)
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_flow_draft_names_one_flow_of_its_own_workspace_and_nothing_else(pool: PgPool) {
        let (one, two) = (workspace(&pool, "one").await, workspace(&pool, "two").await);
        let ada = user(&pool, "ada@example.com").await;
        let mine = flow(&pool, one, "Welcome").await;
        let foreign = flow(&pool, two, "Elsewhere").await;
        let source_id = source(&pool, one).await;
        let now = datetime!(2026-05-15 12:00 UTC);
        let later = now + Duration::days(7);
        let content = json!({"graph": {}, "settings": {}});
        let mut connection = pool.acquire().await.unwrap();

        let good = flow_draft(one, ada, Some(mine), &content, later);
        insert_ai_draft(&mut connection, &good).await.unwrap();
        let found = find_ai_draft(&pool, one, ada, good.id, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.kind, DraftKind::Flow);
        assert_eq!(found.flow_id, Some(mine));
        assert_eq!(DraftKind::parse("flow"), Some(DraftKind::Flow));
        assert_eq!(found.segment_id, None);

        // A flow draft without a flow, of a flow of another workspace, or that also names a
        // source or a segment, is refused; so is a flow named by another kind of draft.
        for refused in [
            flow_draft(one, ada, None, &content, later),
            flow_draft(one, ada, Some(foreign), &content, later),
            NewAiDraft {
                source_id: Some(source_id),
                ..flow_draft(one, ada, Some(mine), &content, later)
            },
            NewAiDraft {
                kind: DraftKind::Segment,
                ..flow_draft(one, ada, Some(mine), &content, later)
            },
        ] {
            assert!(insert_ai_draft(&mut connection, &refused).await.is_err());
        }
    }
}
