//! The `webhooks` rows. The API creates the source, the stream and the sealed secret in the
//! same transaction; deleting the hook is deleting its source, which cascades.

use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{SecretId, UserId, WorkspaceId};

use crate::StoreError;

/// A hook with the name and state of its source and stream.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct Webhook {
    pub id: Uuid,
    pub source_id: Uuid,
    pub stream_id: Uuid,
    pub secret_id: Uuid,
    /// `sources.name`.
    pub name: String,
    pub signature_header: String,
    pub signature_prefix: String,
    /// `None` is the body-only scheme.
    pub timestamp_header: Option<String>,
    pub tolerance_seconds: i32,
    pub dedupe_field: String,
    /// Whether the hook accepts payloads: the source and the stream are enabled and the stream
    /// is mapped.
    pub receiving: bool,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The columns of [`Webhook`], from `webhooks w`, `sources s` and `streams t`.
macro_rules! webhook_query {
    ($tail:literal) => {
        concat!(
            "SELECT w.id, w.source_id, w.stream_id, w.secret_id, s.name, w.signature_header, \
             w.signature_prefix, w.timestamp_header, w.tolerance_seconds, w.dedupe_field, \
             (s.enabled AND t.enabled AND t.mapping IS NOT NULL) AS receiving, w.created_by, \
             w.created_at, w.updated_at \
             FROM webhooks w \
             JOIN sources s ON s.workspace_id = w.workspace_id AND s.id = w.source_id \
             JOIN streams t ON t.workspace_id = w.workspace_id AND t.id = w.stream_id ",
            $tail
        )
    };
}

/// A hook to create. The source, the stream and the secret must already exist in the same
/// transaction.
#[derive(Debug)]
pub struct NewWebhook<'a> {
    /// The random v4 id of the URL.
    pub id: Uuid,
    pub source_id: Uuid,
    pub stream_id: Uuid,
    pub secret_id: SecretId,
    pub signature_header: &'a str,
    pub signature_prefix: &'a str,
    pub timestamp_header: Option<&'a str>,
    pub tolerance_seconds: i32,
    pub dedupe_field: &'a str,
    pub created_by: Option<UserId>,
}

/// The scheme fields that can change; `None` leaves a field as it is.
#[derive(Debug, Default)]
pub struct WebhookUpdate<'a> {
    pub signature_header: Option<&'a str>,
    pub signature_prefix: Option<&'a str>,
    /// `Some(None)` selects the body-only scheme.
    pub timestamp_header: Option<Option<&'a str>>,
    pub tolerance_seconds: Option<i32>,
    pub dedupe_field: Option<&'a str>,
}

/// Inserts the `webhooks` row and returns the hook.
pub async fn create_webhook(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    new: &NewWebhook<'_>,
) -> Result<Webhook, StoreError> {
    sqlx::query(
        "INSERT INTO webhooks (id, workspace_id, source_id, stream_id, secret_id, \
            signature_header, signature_prefix, timestamp_header, tolerance_seconds, \
            dedupe_field, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(new.id)
    .bind(workspace.as_uuid())
    .bind(new.source_id)
    .bind(new.stream_id)
    .bind(new.secret_id.as_uuid())
    .bind(new.signature_header)
    .bind(new.signature_prefix)
    .bind(new.timestamp_header)
    .bind(new.tolerance_seconds)
    .bind(new.dedupe_field)
    .bind(new.created_by.map(|user| user.as_uuid()))
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    find_webhook(&mut *connection, workspace, new.id)
        .await?
        .ok_or_else(StoreError::poison)
}

/// The workspace's hooks, ordered by name.
pub async fn list_webhooks<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
) -> Result<Vec<Webhook>, StoreError> {
    sqlx::query_as(webhook_query!(
        "WHERE w.workspace_id = $1 ORDER BY lower(s.name), w.id"
    ))
    .bind(workspace.as_uuid())
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// One hook of the workspace; another workspace's id is not found.
pub async fn find_webhook<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Webhook>, StoreError> {
    sqlx::query_as(webhook_query!("WHERE w.workspace_id = $1 AND w.id = $2"))
        .bind(workspace.as_uuid())
        .bind(id)
        .fetch_optional(executor)
        .await
        .map_err(StoreError::query)
}

/// Changes the given scheme fields; `None` when the workspace has no such hook. The name goes
/// through `update_source`.
pub async fn update_webhook(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    update: &WebhookUpdate<'_>,
) -> Result<Option<Webhook>, StoreError> {
    let done = sqlx::query(
        "UPDATE webhooks SET signature_header = COALESCE($3, signature_header), \
            signature_prefix = COALESCE($4, signature_prefix), \
            timestamp_header = CASE WHEN $5 THEN $6 ELSE timestamp_header END, \
            tolerance_seconds = COALESCE($7, tolerance_seconds), \
            dedupe_field = COALESCE($8, dedupe_field), updated_at = now() \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(update.signature_header)
    .bind(update.signature_prefix)
    .bind(update.timestamp_header.is_some())
    .bind(update.timestamp_header.flatten())
    .bind(update.tolerance_seconds)
    .bind(update.dedupe_field)
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    if done.rows_affected() == 0 {
        return Ok(None);
    }
    find_webhook(&mut *connection, workspace, id).await
}

/// Points the hook at a new sealed secret and returns the old secret's id, which the caller
/// deletes in the same transaction; `None` when the workspace has no such hook. The new secret
/// must already exist.
pub async fn replace_webhook_secret(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
    new_secret: SecretId,
) -> Result<Option<SecretId>, StoreError> {
    let old: Option<Uuid> = sqlx::query_scalar(
        "SELECT secret_id FROM webhooks WHERE workspace_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let Some(old) = old else {
        return Ok(None);
    };
    sqlx::query(
        "UPDATE webhooks SET secret_id = $3, updated_at = now() \
         WHERE workspace_id = $1 AND id = $2",
    )
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(new_secret.as_uuid())
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    Ok(Some(SecretId::from_uuid(old)))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::super::fixtures::{hook, mapping, workspace};
    use super::*;
    use crate::{begin, commit, delete_source, find_source, secrets};

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hook_is_created_listed_found_and_reports_whether_it_is_receiving(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mapped = hook(&pool, ws, "orders", Some(&mapping(&[]))).await;
        let unmapped = hook(&pool, ws, "forms", None).await;

        assert!(mapped.receiving);
        assert!(!unmapped.receiving);
        assert_eq!(mapped.name, "orders");
        assert_eq!(mapped.signature_prefix, "sha256=");
        assert_eq!(mapped.tolerance_seconds, 300);
        let listed = list_webhooks(&pool, ws).await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|hook| hook.name.as_str())
                .collect::<Vec<_>>(),
            ["forms", "orders"]
        );
        assert_eq!(
            find_webhook(&pool, ws, mapped.id).await.unwrap(),
            Some(mapped)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_hook_is_invisible_and_unchangeable_from_another_workspace(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let mine = hook(&pool, one, "orders", None).await;

        assert!(find_webhook(&pool, two, mine.id).await.unwrap().is_none());
        assert!(list_webhooks(&pool, two).await.unwrap().is_empty());
        let mut connection = pool.acquire().await.unwrap();
        let update = WebhookUpdate {
            dedupe_field: Some("other"),
            ..WebhookUpdate::default()
        };
        assert!(
            update_webhook(&mut connection, two, mine.id, &update)
                .await
                .unwrap()
                .is_none()
        );
        let new_secret = SecretId::new();
        assert!(
            replace_webhook_secret(&mut connection, two, mine.id, new_secret)
                .await
                .unwrap()
                .is_none()
        );
        let unchanged = find_webhook(&pool, one, mine.id).await.unwrap().unwrap();
        assert_eq!(unchanged.dedupe_field, "id");
        assert_eq!(unchanged.secret_id, mine.secret_id);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_scheme_changes_and_the_body_only_scheme_clears_the_timestamp_header(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let created = hook(&pool, ws, "orders", None).await;
        let mut connection = pool.acquire().await.unwrap();

        let changed = update_webhook(
            &mut connection,
            ws,
            created.id,
            &WebhookUpdate {
                signature_header: Some("X-Sig"),
                signature_prefix: Some(""),
                timestamp_header: Some(None),
                tolerance_seconds: Some(60),
                dedupe_field: Some("data.id"),
            },
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(changed.signature_header, "X-Sig");
        assert_eq!(changed.signature_prefix, "");
        assert_eq!(changed.timestamp_header, None);
        assert_eq!(changed.tolerance_seconds, 60);
        assert_eq!(changed.dedupe_field, "data.id");
        let untouched = update_webhook(&mut connection, ws, created.id, &WebhookUpdate::default())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(untouched.timestamp_header, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn replacing_the_secret_returns_the_old_one_and_a_range_check_refuses_bad_values(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let created = hook(&pool, ws, "orders", None).await;
        let keys =
            zirv_flow_core::Keyring::new(zirv_flow_core::MasterKey::generate(), None).unwrap();
        let fresh = SecretId::new();
        let sealed = zirv_flow_core::seal(
            &keys,
            b"zfwh_next",
            &zirv_flow_core::secret_aad(ws, fresh, zirv_flow_core::SecretPurpose::WebhookSecret),
        );
        secrets::insert_unchecked(
            &pool,
            ws,
            zirv_flow_core::SecretPurpose::WebhookSecret,
            fresh,
            &sealed,
        )
        .await
        .unwrap();
        let mut tx = begin(&pool).await.unwrap();

        let old = replace_webhook_secret(&mut tx, ws, created.id, fresh)
            .await
            .unwrap();
        commit(tx).await.unwrap();

        assert_eq!(old.map(|id| id.as_uuid()), Some(created.secret_id));
        let after = find_webhook(&pool, ws, created.id).await.unwrap().unwrap();
        assert_eq!(after.secret_id, fresh.as_uuid());
        let refused = sqlx::query("UPDATE webhooks SET tolerance_seconds = 0 WHERE id = $1")
            .bind(created.id)
            .execute(&pool)
            .await;
        assert!(refused.is_err());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_the_source_deletes_the_hook(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let created = hook(&pool, ws, "orders", None).await;
        let mut connection = pool.acquire().await.unwrap();

        assert!(
            delete_source(&mut connection, ws, created.source_id)
                .await
                .unwrap()
                .is_some()
        );

        assert!(find_webhook(&pool, ws, created.id).await.unwrap().is_none());
        assert!(
            find_source(&pool, ws, created.source_id)
                .await
                .unwrap()
                .is_none()
        );
    }
}
