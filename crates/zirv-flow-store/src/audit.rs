//! The append-only audit log. This module offers appending and reading only; nothing
//! updates or deletes a row.

use serde_json::Value;
use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

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
    pub summary: Value,
    pub occurred_at: OffsetDateTime,
}

pub async fn append_audit<'e>(
    executor: impl PgExecutor<'e>,
    entry: &NewAuditEntry,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO audit_log \
            (workspace_id, actor_kind, actor_id, action, object_kind, object_id, request_id, summary) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(entry.workspace_id.map(|id| id.as_uuid()))
    .bind(entry.actor.kind.as_str())
    .bind(entry.actor.id)
    .bind(entry.action)
    .bind(entry.object_kind)
    .bind(entry.object_id)
    .bind(entry.request_id.as_deref())
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
                request_id, summary, occurred_at \
         FROM audit_log ORDER BY id DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;

    use super::*;

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
        for action in ["first.action", "second.action"] {
            append_audit(
                &pool,
                &NewAuditEntry {
                    workspace_id: None,
                    actor: Actor::user(user),
                    action,
                    object_kind: Some("thing"),
                    object_id: None,
                    request_id: Some("req-1".into()),
                    summary: json!({"reason": "test"}),
                },
            )
            .await
            .unwrap();
        }

        let rows = list_audit(&pool, 10).await.unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].action, "second.action");
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
}
