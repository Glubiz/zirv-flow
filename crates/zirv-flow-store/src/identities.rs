//! Links between users and external (OIDC) identities, and the state of logins in progress.

use std::fmt;

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use zirv_flow_core::UserId;

use crate::StoreError;
use crate::accounts::{User, UserRow};

/// The server-side half of an OIDC login in progress.
#[derive(Clone)]
pub struct OidcLogin {
    pub nonce: String,
    pub pkce_verifier: String,
}

// The nonce and the verifier belong to a login in progress: neither is printed.
impl fmt::Debug for OidcLogin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("OidcLogin").finish_non_exhaustive()
    }
}

#[derive(FromRow)]
struct OidcLoginRow {
    nonce: String,
    pkce_verifier: String,
}

/// Links an identity (issuer and subject) to a user. An identity already linked, or a user
/// who already has an identity at this issuer, is a conflict.
pub async fn link_identity<'e>(
    executor: impl PgExecutor<'e>,
    issuer: &str,
    subject: &str,
    user: UserId,
) -> Result<(), StoreError> {
    sqlx::query("INSERT INTO user_identities (issuer, subject, user_id) VALUES ($1, $2, $3)")
        .bind(issuer)
        .bind(subject)
        .bind(user.as_uuid())
        .execute(executor)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// The user an identity is linked to.
pub async fn find_identity_user<'e>(
    executor: impl PgExecutor<'e>,
    issuer: &str,
    subject: &str,
) -> Result<Option<User>, StoreError> {
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT u.id, u.email, u.password_hash, u.is_owner, u.failed_sign_ins, u.locked_until, \
                u.created_at \
         FROM user_identities i JOIN users u ON u.id = i.user_id \
         WHERE i.issuer = $1 AND i.subject = $2",
    )
    .bind(issuer)
    .bind(subject)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(Into::into))
}

/// Stores a login in progress, valid for `ttl_seconds` by the database clock.
pub async fn create_oidc_login<'e>(
    executor: impl PgExecutor<'e>,
    state_hash: &[u8],
    nonce: &str,
    pkce_verifier: &str,
    ttl_seconds: i64,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO oidc_logins (state_hash, nonce, pkce_verifier, expires_at) \
         VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
    )
    .bind(state_hash)
    .bind(nonce)
    .bind(pkce_verifier)
    .bind(ttl_seconds as f64)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Takes a login in progress: it is deleted as it is read, so it can be used once, and an
/// expired one is not returned.
pub async fn take_oidc_login<'e>(
    executor: impl PgExecutor<'e>,
    state_hash: &[u8],
) -> Result<Option<OidcLogin>, StoreError> {
    let row: Option<OidcLoginRow> = sqlx::query_as(
        "DELETE FROM oidc_logins WHERE state_hash = $1 AND expires_at > now() \
         RETURNING nonce, pkce_verifier",
    )
    .bind(state_hash)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(|row| OidcLogin {
        nonce: row.nonce,
        pkce_verifier: row.pkce_verifier,
    }))
}

/// Removes logins that were never completed.
pub async fn delete_expired_oidc_logins<'e>(
    executor: impl PgExecutor<'e>,
) -> Result<u64, StoreError> {
    let result = sqlx::query("DELETE FROM oidc_logins WHERE expires_at <= now()")
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

    async fn user(pool: &PgPool, email: &str) -> User {
        create_user(pool, &Email::parse(email).unwrap(), None, false)
            .await
            .unwrap()
    }

    #[test]
    fn debug_output_hides_the_nonce_and_the_verifier() {
        let login = OidcLogin {
            nonce: "nonce-secret".into(),
            pkce_verifier: "verifier-secret".into(),
        };

        let debug = format!("{login:?}");

        assert!(!debug.contains("secret"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_user_without_a_password_is_found_through_its_identity(pool: PgPool) {
        let jane = user(&pool, "jane@example.com").await;
        assert!(jane.password_hash.is_none());

        link_identity(&pool, "https://idp", "sub-1", jane.id)
            .await
            .unwrap();

        let found = find_identity_user(&pool, "https://idp", "sub-1")
            .await
            .unwrap();
        assert_eq!(found.unwrap().id, jane.id);
        assert!(
            find_identity_user(&pool, "https://idp", "sub-2")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            find_identity_user(&pool, "https://other", "sub-1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_identity_and_a_user_per_issuer_can_be_linked_only_once(pool: PgPool) {
        let jane = user(&pool, "jane@example.com").await;
        let john = user(&pool, "john@example.com").await;
        link_identity(&pool, "https://idp", "sub-1", jane.id)
            .await
            .unwrap();

        let taken = link_identity(&pool, "https://idp", "sub-1", john.id)
            .await
            .unwrap_err();
        let second = link_identity(&pool, "https://idp", "sub-2", jane.id)
            .await
            .unwrap_err();

        assert!(taken.is_conflict());
        assert!(second.is_conflict());
        link_identity(&pool, "https://other", "sub-1", jane.id)
            .await
            .unwrap();
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn deleting_a_user_removes_its_identities(pool: PgPool) {
        let jane = user(&pool, "jane@example.com").await;
        link_identity(&pool, "https://idp", "sub-1", jane.id)
            .await
            .unwrap();

        sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(jane.id.as_uuid())
            .execute(&pool)
            .await
            .unwrap();

        assert!(
            find_identity_user(&pool, "https://idp", "sub-1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_login_can_be_taken_once(pool: PgPool) {
        create_oidc_login(&pool, b"hash", "nonce", "verifier", 600)
            .await
            .unwrap();

        let first = take_oidc_login(&pool, b"hash").await.unwrap().unwrap();
        let second = take_oidc_login(&pool, b"hash").await.unwrap();

        assert_eq!(first.nonce, "nonce");
        assert_eq!(first.pkce_verifier, "verifier");
        assert!(second.is_none());
        assert!(take_oidc_login(&pool, b"unknown").await.unwrap().is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_expired_login_cannot_be_taken_and_is_cleaned_up(pool: PgPool) {
        create_oidc_login(&pool, b"old", "n", "v", -1)
            .await
            .unwrap();
        create_oidc_login(&pool, b"fresh", "n", "v", 600)
            .await
            .unwrap();

        assert!(take_oidc_login(&pool, b"old").await.unwrap().is_none());
        create_oidc_login(&pool, b"old2", "n", "v", -1)
            .await
            .unwrap();
        assert_eq!(delete_expired_oidc_logins(&pool).await.unwrap(), 2);
        assert!(take_oidc_login(&pool, b"fresh").await.unwrap().is_some());
    }
}
