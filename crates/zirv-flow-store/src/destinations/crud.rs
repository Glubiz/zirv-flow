//! Creating, reading, changing and archiving destinations. Every function is scoped to one
//! workspace. Creating a destination creates its rate bucket in the same statement.

use serde_json::Value;
use sqlx::prelude::FromRow;
use sqlx::{PgConnection, PgExecutor};
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{UserId, WorkspaceId};

use crate::StoreError;

/// A configured destination. The credential is a reference to a workspace credential, never a
/// value.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct Destination {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    pub config: Value,
    /// The workspace credential the destination uses.
    pub credential_id: Option<Uuid>,
    /// The sealed secret of that credential at the time of the read; derived, not stored here.
    /// Open the credential with [`open_credential`](crate::open_credential).
    pub credential_secret_id: Option<Uuid>,
    /// The names of the fields the credential holds; never their values. Derived.
    pub credential_fields: Vec<String>,
    /// Deliveries per second the destination tolerates.
    pub rate: i32,
    /// The most deliveries it may take at once.
    pub burst: i32,
    pub archived_at: Option<OffsetDateTime>,
    pub created_by: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The columns of [`Destination`].
macro_rules! destination_columns {
    () => {
        "id, kind, name, config, credential_id, \
         credential_secret_of(workspace_id, credential_id) AS credential_secret_id, \
         credential_field_names(workspace_id, credential_id) AS credential_fields, rate, burst, \
         archived_at, created_by, created_at, updated_at"
    };
}

/// A destination to create. The credential, if any, must be a live credential of the workspace;
/// lock it with [`lock_credential_for_use`](crate::lock_credential_for_use) in the same
/// transaction.
#[derive(Debug)]
pub struct NewDestination<'a> {
    pub id: Uuid,
    pub kind: &'a str,
    pub name: &'a str,
    pub config: &'a Value,
    pub credential_id: Option<Uuid>,
    pub rate: i32,
    pub burst: i32,
    pub created_by: Option<UserId>,
}

/// The fields of a destination that can change; `None` leaves a field as it is.
#[derive(Debug, Default)]
pub struct DestinationUpdate<'a> {
    pub name: Option<&'a str>,
    pub config: Option<&'a Value>,
    pub rate: Option<i32>,
    pub burst: Option<i32>,
    /// Links another workspace credential.
    pub credential_id: Option<Uuid>,
}

/// Creates a destination and its full rate bucket. A name already taken by a live destination in
/// the workspace is a conflict.
pub async fn create_destination<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    new: &NewDestination<'_>,
) -> Result<Destination, StoreError> {
    sqlx::query_as(concat!(
        "WITH d AS ( \
            INSERT INTO destinations (id, workspace_id, kind, name, config, credential_id, \
                rate, burst, created_by) \
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING *), \
         b AS ( \
            INSERT INTO destination_rate_buckets \
                (destination_id, workspace_id, rate, burst, tokens, refilled_at) \
            SELECT id, workspace_id, rate, burst, burst, clock_timestamp() FROM d) \
         SELECT ",
        destination_columns!(),
        " FROM d"
    ))
    .bind(new.id)
    .bind(workspace.as_uuid())
    .bind(new.kind)
    .bind(new.name)
    .bind(new.config)
    .bind(new.credential_id)
    .bind(new.rate)
    .bind(new.burst)
    .bind(new.created_by.map(|user| user.as_uuid()))
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)
}

/// The workspace's destinations, ordered by name; archived ones only when asked for.
pub async fn list_destinations<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    include_archived: bool,
) -> Result<Vec<Destination>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        destination_columns!(),
        " FROM destinations WHERE workspace_id = $1 AND ($2 OR archived_at IS NULL) \
          ORDER BY lower(name), id"
    ))
    .bind(workspace.as_uuid())
    .bind(include_archived)
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)
}

/// One destination, archived or not.
pub async fn find_destination<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Destination>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        destination_columns!(),
        " FROM destinations WHERE workspace_id = $1 AND id = $2"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// The workspace of the destination with this id, whichever workspace it is in. For the one
/// public route that receives a provider's webhook by the destination's id; the id only locates
/// the destination, and a signature authorizes. Archived destinations are found too.
pub async fn find_destination_workspace<'e>(
    executor: impl PgExecutor<'e>,
    id: Uuid,
) -> Result<Option<WorkspaceId>, StoreError> {
    let workspace: Option<Uuid> =
        sqlx::query_scalar("SELECT workspace_id FROM destinations WHERE id = $1")
            .bind(id)
            .fetch_optional(executor)
            .await
            .map_err(StoreError::query)?;
    Ok(workspace.map(WorkspaceId::from_uuid))
}

/// Reads a destination and locks its row until the transaction ends, so concurrent changes to it
/// (a credential replaced twice, say) run one after the other.
pub async fn find_destination_for_update(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Destination>, StoreError> {
    sqlx::query_as(concat!(
        "SELECT ",
        destination_columns!(),
        " FROM destinations WHERE workspace_id = $1 AND id = $2 FOR UPDATE"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(connection)
    .await
    .map_err(StoreError::query)
}

/// Changes the given fields of a live destination. `None` when there is no such live
/// destination; a name taken by another live destination is a conflict. The bucket follows a
/// changed rate or burst at its next take, and the bucket is reset in the same statement:
/// its burst follows and its tokens are clamped to the new burst. Leases already granted run out
/// within one lease.
pub async fn update_destination<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
    update: &DestinationUpdate<'_>,
) -> Result<Option<Destination>, StoreError> {
    sqlx::query_as(concat!(
        "WITH d AS ( \
            UPDATE destinations SET name = COALESCE($3, name), config = COALESCE($4, config), \
                rate = COALESCE($5, rate), burst = COALESCE($6, burst), \
                credential_id = COALESCE($7, credential_id), updated_at = now() \
            WHERE workspace_id = $1 AND id = $2 AND archived_at IS NULL RETURNING *), \
         b AS ( \
            UPDATE destination_rate_buckets b SET burst = d.burst, \
                rate = LEAST(b.rate, d.rate::float8), tokens = LEAST(b.tokens, d.burst::float8) \
            FROM d WHERE b.destination_id = d.id) \
         SELECT ",
        destination_columns!(),
        " FROM d"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .bind(update.name)
    .bind(update.config)
    .bind(update.rate)
    .bind(update.burst)
    .bind(update.credential_id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Archives a live destination and returns it. `None` when there is no such live destination. Its
/// rate bucket is deleted in the same statement, so nothing more can be taken for it; attempts keep
/// their history, and the name is free again.
pub async fn archive_destination<'e>(
    executor: impl PgExecutor<'e>,
    workspace: WorkspaceId,
    id: Uuid,
) -> Result<Option<Destination>, StoreError> {
    sqlx::query_as(concat!(
        "WITH d AS ( \
            UPDATE destinations SET archived_at = now(), updated_at = now() \
            WHERE workspace_id = $1 AND id = $2 AND archived_at IS NULL RETURNING *), \
         gone AS ( \
            DELETE FROM destination_rate_buckets b USING d WHERE b.destination_id = d.id) \
         SELECT ",
        destination_columns!(),
        " FROM d"
    ))
    .bind(workspace.as_uuid())
    .bind(id)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sqlx::PgPool;
    use zirv_flow_core::{Slug, WorkspaceName};

    use super::*;
    use crate::create_workspace;
    use crate::workspace_credentials::testing::credential_with_fields;

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    fn new<'a>(name: &'a str, config: &'a Value) -> NewDestination<'a> {
        NewDestination {
            id: Uuid::now_v7(),
            kind: "fake",
            name,
            config,
            credential_id: None,
            rate: 10,
            burst: 10,
            created_by: None,
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_destination_is_created_with_a_full_bucket_read_listed_updated_and_archived(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let config = json!({"mode": "ok"});

        let created = create_destination(&pool, ws, &new("Beta", &config))
            .await
            .unwrap();
        create_destination(&pool, ws, &new("alpha", &config))
            .await
            .unwrap();

        assert_eq!(created.config, config);
        assert_eq!((created.rate, created.burst), (10, 10));
        assert!(created.archived_at.is_none());
        let bucket = crate::bucket_state(&pool, created.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((bucket.rate, bucket.burst, bucket.tokens), (10.0, 10, 10.0));
        assert!(bucket.penalty_until.is_none());
        assert_eq!(
            find_destination(&pool, ws, created.id)
                .await
                .unwrap()
                .unwrap(),
            created
        );
        let names: Vec<_> = list_destinations(&pool, ws, false)
            .await
            .unwrap()
            .into_iter()
            .map(|destination| destination.name)
            .collect();
        assert_eq!(names, ["alpha", "Beta"]);

        let more = json!({"mode": "throttle"});
        let credential = credential_with_fields(&pool, ws, "mail", &["token"]).await;
        let updated = update_destination(
            &pool,
            ws,
            created.id,
            &DestinationUpdate {
                name: Some("Gamma"),
                config: Some(&more),
                rate: Some(20),
                burst: Some(30),
                credential_id: Some(credential.id),
            },
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(updated.name, "Gamma");
        assert_eq!(updated.config, more);
        assert_eq!((updated.rate, updated.burst), (20, 30));
        assert_eq!(updated.credential_fields, ["token"]);
        assert!(updated.updated_at >= created.updated_at);
        assert_eq!(updated.created_at, created.created_at);

        let archived = archive_destination(&pool, ws, created.id)
            .await
            .unwrap()
            .unwrap();
        assert!(archived.archived_at.is_some());
        assert!(
            archive_destination(&pool, ws, created.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            update_destination(&pool, ws, created.id, &DestinationUpdate::default())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(list_destinations(&pool, ws, false).await.unwrap().len(), 1);
        assert_eq!(list_destinations(&pool, ws, true).await.unwrap().len(), 2);
        assert!(
            find_destination(&pool, ws, created.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_live_name_is_unique_per_workspace_ignoring_case_and_an_archived_name_is_free(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let config = json!({});

        let first = create_destination(&pool, ws, &new("Mail", &config))
            .await
            .unwrap();
        let clash = create_destination(&pool, ws, &new("MAIL", &config))
            .await
            .unwrap_err();
        create_destination(&pool, other, &new("Mail", &config))
            .await
            .unwrap();

        assert!(clash.is_conflict());
        archive_destination(&pool, ws, first.id).await.unwrap();
        create_destination(&pool, ws, &new("Mail", &config))
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_store_refuses_a_secret_of_another_workspace_and_a_locked_row_makes_a_second_locker_wait(
        pool: PgPool,
    ) {
        let ws = workspace(&pool, "one").await;
        let other = workspace(&pool, "two").await;
        let config = json!({});
        let foreign = credential_with_fields(&pool, other, "mail", &["token"]).await;
        let own = credential_with_fields(&pool, ws, "mail", &["token"]).await;

        let mut refused = new("A", &config);
        refused.credential_id = Some(foreign.id);
        assert!(create_destination(&pool, ws, &refused).await.is_err());
        let mut linked = new("B", &config);
        linked.credential_id = Some(own.id);
        let created = create_destination(&pool, ws, &linked).await.unwrap();
        assert_eq!(created.credential_secret_id, Some(own.secret_id));

        let mut first = crate::begin(&pool).await.unwrap();
        find_destination_for_update(&mut first, ws, created.id)
            .await
            .unwrap()
            .unwrap();
        let pool2 = pool.clone();
        let second = tokio::spawn(async move {
            let mut tx = crate::begin(&pool2).await.unwrap();
            find_destination_for_update(&mut tx, ws, created.id)
                .await
                .unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!second.is_finished(), "the second locker waits");
        crate::commit(first).await.unwrap();
        second.await.unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_a_workspace_deletes_its_destinations_buckets_and_attempts(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let config = json!({});
        let created = create_destination(&pool, ws, &new("Mail", &config))
            .await
            .unwrap();
        crate::record_delivery_attempt(
            &pool,
            ws,
            &crate::NewDeliveryAttempt {
                destination_id: created.id,
                subject_kind: crate::SubjectKind::Message,
                subject_id: Uuid::now_v7(),
                attempt: 1,
                outcome: crate::AttemptOutcome::Delivered,
                status_class: crate::StatusClass::Success,
                duration_ms: 5,
            },
        )
        .await
        .unwrap();

        sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(ws.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        for table in [
            "destinations",
            "destination_rate_buckets",
            "delivery_attempts",
        ] {
            let rows: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(rows, 0, "{table}");
        }
    }
}
