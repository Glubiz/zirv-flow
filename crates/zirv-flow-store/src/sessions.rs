//! Server-side sessions. Only hashes of the session token and the CSRF token are stored.

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::UserId;

use crate::StoreError;
use crate::accounts::{User, UserRow};

/// A live session together with its user.
#[derive(Clone, Debug)]
pub struct ActiveSession {
    pub user: User,
    pub csrf_hash: Vec<u8>,
    pub last_seen_at: OffsetDateTime,
}

#[derive(FromRow)]
struct ActiveRow {
    csrf_hash: Vec<u8>,
    last_seen_at: OffsetDateTime,
    id: Uuid,
    email: String,
    password_hash: Option<String>,
    is_owner: bool,
    failed_sign_ins: i32,
    locked_until: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
}

/// Stores a new session that is valid for `absolute_seconds`.
pub async fn create_session<'e>(
    executor: impl PgExecutor<'e>,
    token_hash: &[u8],
    user: UserId,
    csrf_hash: &[u8],
    absolute_seconds: i64,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, csrf_hash, expires_at) \
         VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
    )
    .bind(token_hash)
    .bind(user.as_uuid())
    .bind(csrf_hash)
    .bind(absolute_seconds as f64)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// The session for a token hash, if it has neither passed its absolute expiry nor been
/// idle for longer than `idle_seconds`.
pub async fn find_active_session<'e>(
    executor: impl PgExecutor<'e>,
    token_hash: &[u8],
    idle_seconds: i64,
) -> Result<Option<ActiveSession>, StoreError> {
    let row: Option<ActiveRow> = sqlx::query_as(
        "SELECT s.csrf_hash, s.last_seen_at, u.id, u.email, u.password_hash, u.is_owner, \
                u.failed_sign_ins, u.locked_until, u.created_at \
         FROM sessions s JOIN users u ON u.id = s.user_id \
         WHERE s.token_hash = $1 AND s.expires_at > now() \
           AND s.last_seen_at > now() - make_interval(secs => $2)",
    )
    .bind(token_hash)
    .bind(idle_seconds as f64)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(|row| ActiveSession {
        csrf_hash: row.csrf_hash,
        last_seen_at: row.last_seen_at,
        user: UserRow {
            id: row.id,
            email: row.email,
            password_hash: row.password_hash,
            is_owner: row.is_owner,
            failed_sign_ins: row.failed_sign_ins,
            locked_until: row.locked_until,
            created_at: row.created_at,
        }
        .into(),
    }))
}

/// Refreshes `last_seen_at`, at most once a minute per session.
pub async fn touch_session<'e>(
    executor: impl PgExecutor<'e>,
    token_hash: &[u8],
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE sessions SET last_seen_at = now() \
         WHERE token_hash = $1 AND last_seen_at < now() - interval '1 minute'",
    )
    .bind(token_hash)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Ends one session. Returns whether it existed.
pub async fn delete_session<'e>(
    executor: impl PgExecutor<'e>,
    token_hash: &[u8],
) -> Result<bool, StoreError> {
    let result = sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(token_hash)
        .execute(executor)
        .await
        .map_err(StoreError::query)?;
    Ok(result.rows_affected() == 1)
}

/// Ends every session of a user, for example after a password reset.
pub async fn delete_sessions_for_user<'e>(
    executor: impl PgExecutor<'e>,
    user: UserId,
) -> Result<u64, StoreError> {
    let result = sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user.as_uuid())
        .execute(executor)
        .await
        .map_err(StoreError::query)?;
    Ok(result.rows_affected())
}

/// Removes sessions past their absolute expiry or idle for longer than `idle_seconds`.
pub async fn delete_expired_sessions<'e>(
    executor: impl PgExecutor<'e>,
    idle_seconds: i64,
) -> Result<u64, StoreError> {
    let result = sqlx::query(
        "DELETE FROM sessions WHERE expires_at <= now() \
         OR last_seen_at <= now() - make_interval(secs => $1)",
    )
    .bind(idle_seconds as f64)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use zirv_flow_core::Email;

    use super::*;
    use crate::create_user;

    const IDLE: i64 = 12 * 3600;
    const ABSOLUTE: i64 = 7 * 24 * 3600;

    async fn user(pool: &PgPool) -> UserId {
        create_user(
            pool,
            &Email::parse("a@example.com").unwrap(),
            Some("hash"),
            false,
        )
        .await
        .unwrap()
        .id
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_new_session_is_found_with_its_user(pool: PgPool) {
        let id = user(&pool).await;
        create_session(&pool, b"token", id, b"csrf", ABSOLUTE)
            .await
            .unwrap();

        let session = find_active_session(&pool, b"token", IDLE)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(session.user.id, id);
        assert_eq!(session.csrf_hash, b"csrf");
        assert!(
            find_active_session(&pool, b"other", IDLE)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_session_past_its_absolute_expiry_is_not_found(pool: PgPool) {
        let id = user(&pool).await;
        create_session(&pool, b"token", id, b"csrf", ABSOLUTE)
            .await
            .unwrap();
        sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();

        assert!(
            find_active_session(&pool, b"token", IDLE)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_idle_session_is_not_found(pool: PgPool) {
        let id = user(&pool).await;
        create_session(&pool, b"token", id, b"csrf", ABSOLUTE)
            .await
            .unwrap();
        sqlx::query("UPDATE sessions SET last_seen_at = now() - interval '13 hours'")
            .execute(&pool)
            .await
            .unwrap();

        assert!(
            find_active_session(&pool, b"token", IDLE)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn touching_refreshes_at_most_once_a_minute(pool: PgPool) {
        let id = user(&pool).await;
        create_session(&pool, b"token", id, b"csrf", ABSOLUTE)
            .await
            .unwrap();
        let last_seen = |pool: PgPool| async move {
            sqlx::query_scalar::<_, OffsetDateTime>("SELECT last_seen_at FROM sessions")
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        let first = last_seen(pool.clone()).await;

        touch_session(&pool, b"token").await.unwrap();
        assert_eq!(last_seen(pool.clone()).await, first);

        sqlx::query("UPDATE sessions SET last_seen_at = now() - interval '2 minutes'")
            .execute(&pool)
            .await
            .unwrap();
        touch_session(&pool, b"token").await.unwrap();
        let session = find_active_session(&pool, b"token", 60).await.unwrap();
        assert!(session.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn sessions_can_be_deleted_singly_per_user_and_by_expiry(pool: PgPool) {
        let id = user(&pool).await;
        for token in [b"one".as_slice(), b"two", b"three"] {
            create_session(&pool, token, id, b"csrf", ABSOLUTE)
                .await
                .unwrap();
        }

        assert!(delete_session(&pool, b"one").await.unwrap());
        assert!(!delete_session(&pool, b"one").await.unwrap());
        sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second' WHERE token_hash = 'two'::bytea")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(delete_expired_sessions(&pool, IDLE).await.unwrap(), 1);
        assert_eq!(delete_sessions_for_user(&pool, id).await.unwrap(), 1);
    }
}
