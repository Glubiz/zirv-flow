//! The workspace's webhook signing secret (architecture 9.1 and 16.2): which sealed
//! `encrypted_secrets` row is current and which one a rotation retired. The API seals the secret
//! and calls these in the same transaction; the `webhook.call` job reads the live ones, opens
//! them and signs with each.
//!
//! A rotation retires the current secret, which keeps signing for the grace window
//! ([`zirv_flow_core::flow::SIGNING_GRACE_SECONDS`]), and deletes any secret an earlier rotation
//! retired: a second rotation inside the window ends the first secret at once, which is what an
//! operator who suspects a leak wants. Every function runs in the caller's transaction.

use sqlx::PgConnection;
use sqlx::prelude::FromRow;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;
use zirv_flow_core::flow::SIGNING_GRACE_SECONDS;
use zirv_flow_core::{SecretId, WorkspaceId};

use crate::StoreError;

/// A signing secret's row: when it was made and, for a retired one, when it was replaced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SigningKey {
    pub secret_id: SecretId,
    pub created_at: OffsetDateTime,
    pub retired_at: Option<OffsetDateTime>,
}

impl SigningKey {
    /// When a retired key stops signing; none for the current key.
    pub fn valid_until(&self) -> Option<OffsetDateTime> {
        self.retired_at
            .map(|retired| retired + Duration::seconds(SIGNING_GRACE_SECONDS))
    }
}

#[derive(FromRow)]
struct KeyRow {
    secret_id: Uuid,
    created_at: OffsetDateTime,
    retired_at: Option<OffsetDateTime>,
}

impl From<KeyRow> for SigningKey {
    fn from(row: KeyRow) -> Self {
        Self {
            secret_id: SecretId::from_uuid(row.secret_id),
            created_at: row.created_at,
            retired_at: row.retired_at,
        }
    }
}

/// Makes the secret `secret` (already stored in `encrypted_secrets`) the workspace's current
/// signing secret. False when the workspace already has one: nothing is written.
pub async fn create_signing_key(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    secret: SecretId,
    now: OffsetDateTime,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "INSERT INTO webhook_signing_keys (secret_id, workspace_id, created_at) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (workspace_id) WHERE retired_at IS NULL DO NOTHING",
    )
    .bind(secret.as_uuid())
    .bind(workspace.as_uuid())
    .bind(now)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(result.rows_affected() == 1)
}

/// What a rotation changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rotation {
    /// The secret that was current; it signs until [`SigningKey::valid_until`].
    pub retired: SigningKey,
    /// How many earlier retired secrets were deleted, which ended them at once.
    pub dropped: u64,
}

/// Retires the current secret and makes `fresh` (already stored in `encrypted_secrets`) current.
/// Deletes the secrets of every earlier retirement. `None` when the workspace has no current
/// secret to rotate; nothing is written then.
pub async fn rotate_signing_key(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    fresh: SecretId,
    now: OffsetDateTime,
) -> Result<Option<Rotation>, StoreError> {
    let current: Option<Uuid> = sqlx::query_scalar(
        "SELECT secret_id FROM webhook_signing_keys \
         WHERE workspace_id = $1 AND retired_at IS NULL FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    let Some(current) = current else {
        return Ok(None);
    };
    // Deleting the sealed secret deletes its key row with it.
    let dropped = sqlx::query(
        "DELETE FROM encrypted_secrets WHERE workspace_id = $1 AND id IN \
            (SELECT secret_id FROM webhook_signing_keys \
             WHERE workspace_id = $1 AND retired_at IS NOT NULL)",
    )
    .bind(workspace.as_uuid())
    .execute(&mut *connection)
    .await
    .map_err(StoreError::query)?
    .rows_affected();
    let retired: KeyRow = sqlx::query_as(
        "UPDATE webhook_signing_keys SET retired_at = $3 \
         WHERE workspace_id = $1 AND secret_id = $2 \
         RETURNING secret_id, created_at, retired_at",
    )
    .bind(workspace.as_uuid())
    .bind(current)
    .bind(now)
    .fetch_one(&mut *connection)
    .await
    .map_err(StoreError::query)?;
    sqlx::query(
        "INSERT INTO webhook_signing_keys (secret_id, workspace_id, created_at) \
         VALUES ($1, $2, $3)",
    )
    .bind(fresh.as_uuid())
    .bind(workspace.as_uuid())
    .bind(now)
    .execute(connection)
    .await
    .map_err(StoreError::query)?;
    Ok(Some(Rotation {
        retired: retired.into(),
        dropped,
    }))
}

/// The workspace's current secret and, when a rotation left one inside its grace window at
/// `now`, the previous one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SigningKeys {
    pub current: SigningKey,
    pub previous: Option<SigningKey>,
}

/// The secrets that sign at `now`: the current one and the previous one while its window lasts.
/// `None` when the workspace has no signing secret.
pub async fn live_signing_keys(
    connection: &mut PgConnection,
    workspace: WorkspaceId,
    now: OffsetDateTime,
) -> Result<Option<SigningKeys>, StoreError> {
    let rows: Vec<KeyRow> = sqlx::query_as(
        "SELECT secret_id, created_at, retired_at FROM webhook_signing_keys \
         WHERE workspace_id = $1 \
           AND (retired_at IS NULL OR retired_at + make_interval(secs => $3) > $2) \
         ORDER BY retired_at IS NOT NULL, created_at DESC",
    )
    .bind(workspace.as_uuid())
    .bind(now)
    .bind(SIGNING_GRACE_SECONDS as f64)
    .fetch_all(connection)
    .await
    .map_err(StoreError::query)?;
    let mut keys = rows.into_iter().map(SigningKey::from);
    let Some(current) = keys.next().filter(|key| key.retired_at.is_none()) else {
        return Ok(None);
    };
    Ok(Some(SigningKeys {
        current,
        previous: keys.next(),
    }))
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::SecretPurpose;
    use zirv_flow_core::{Keyring, MasterKey, seal, secret_aad};

    use super::*;
    use crate::flows::tests_support::workspace;
    use crate::secrets;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    /// Stores a sealed secret and returns its id.
    async fn stored(pool: &PgPool, ws: WorkspaceId) -> SecretId {
        let keyring = Keyring::new(MasterKey::generate(), None).unwrap();
        let id = SecretId::new();
        let sealed = seal(
            &keyring,
            b"zfwh_x",
            &secret_aad(ws, id, SecretPurpose::WebhookSigning),
        );
        secrets::insert_unchecked(pool, ws, SecretPurpose::WebhookSigning, id, &sealed)
            .await
            .unwrap();
        id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_workspace_has_one_current_secret_and_a_second_create_writes_nothing(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut connection = pool.acquire().await.unwrap();
        assert_eq!(
            live_signing_keys(&mut connection, ws, now()).await.unwrap(),
            None
        );
        let first = stored(&pool, ws).await;
        let second = stored(&pool, ws).await;

        assert!(
            create_signing_key(&mut connection, ws, first, now())
                .await
                .unwrap()
        );
        assert!(
            !create_signing_key(&mut connection, ws, second, now())
                .await
                .unwrap()
        );

        let keys = live_signing_keys(&mut connection, ws, now())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(keys.current.secret_id, first);
        assert_eq!(keys.previous, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_rotation_keeps_the_previous_secret_for_the_window_and_not_after(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut connection = pool.acquire().await.unwrap();
        let old = stored(&pool, ws).await;
        create_signing_key(&mut connection, ws, old, now())
            .await
            .unwrap();
        let fresh = stored(&pool, ws).await;

        let rotation = rotate_signing_key(&mut connection, ws, fresh, now())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(rotation.retired.secret_id, old);
        assert_eq!(
            rotation.retired.valid_until(),
            Some(now() + Duration::seconds(SIGNING_GRACE_SECONDS))
        );
        let inside = now() + Duration::seconds(SIGNING_GRACE_SECONDS - 1);
        let keys = live_signing_keys(&mut connection, ws, inside)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(keys.current.secret_id, fresh);
        assert_eq!(keys.previous.map(|key| key.secret_id), Some(old));
        let after = now() + Duration::seconds(SIGNING_GRACE_SECONDS);
        let keys = live_signing_keys(&mut connection, ws, after)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(keys.current.secret_id, fresh);
        assert_eq!(keys.previous, None);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_second_rotation_inside_the_window_ends_the_first_secret_at_once(pool: PgPool) {
        let ws = workspace(&pool, "one").await;
        let mut connection = pool.acquire().await.unwrap();
        let first = stored(&pool, ws).await;
        create_signing_key(&mut connection, ws, first, now())
            .await
            .unwrap();
        let second = stored(&pool, ws).await;
        rotate_signing_key(&mut connection, ws, second, now())
            .await
            .unwrap();
        let third = stored(&pool, ws).await;

        let rotation = rotate_signing_key(&mut connection, ws, third, now())
            .await
            .unwrap()
            .unwrap();

        assert_eq!((rotation.retired.secret_id, rotation.dropped), (second, 1));
        let keys = live_signing_keys(&mut connection, ws, now())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(keys.current.secret_id, third);
        assert_eq!(keys.previous.map(|key| key.secret_id), Some(second));
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM encrypted_secrets WHERE workspace_id = $1")
                .bind(ws.as_uuid())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 2, "the first secret's row is gone");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn rotating_without_a_secret_writes_nothing_and_workspaces_stay_apart(pool: PgPool) {
        let one = workspace(&pool, "one").await;
        let two = workspace(&pool, "two").await;
        let mut connection = pool.acquire().await.unwrap();
        let fresh = stored(&pool, one).await;

        assert_eq!(
            rotate_signing_key(&mut connection, one, fresh, now())
                .await
                .unwrap(),
            None
        );
        create_signing_key(&mut connection, one, fresh, now())
            .await
            .unwrap();

        assert_eq!(
            live_signing_keys(&mut connection, two, now())
                .await
                .unwrap(),
            None
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM webhook_signing_keys")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }
}
