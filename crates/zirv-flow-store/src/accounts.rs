//! Local accounts.

use std::fmt;

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;
use zirv_flow_core::{Email, UserId};

use crate::StoreError;

macro_rules! user_columns {
    () => {
        "id, email, password_hash, is_owner, failed_sign_ins, locked_until, created_at"
    };
}

/// Failed sign-ins in a row before an account is locked.
const LOCK_THRESHOLD: i32 = 5;
/// The longest a lock lasts, in seconds.
const MAX_LOCK_SECONDS: f64 = 900.0;

/// A local account.
#[derive(Clone)]
pub struct User {
    pub id: UserId,
    pub email: String,
    pub password_hash: String,
    pub is_owner: bool,
    pub failed_sign_ins: i32,
    pub locked_until: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

// The address is personal data and the hash is a credential: neither is printed.
impl fmt::Debug for User {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("User")
            .field("id", &self.id)
            .field("is_owner", &self.is_owner)
            .finish_non_exhaustive()
    }
}

#[derive(FromRow)]
pub(crate) struct UserRow {
    pub(crate) id: Uuid,
    pub(crate) email: String,
    pub(crate) password_hash: String,
    pub(crate) is_owner: bool,
    pub(crate) failed_sign_ins: i32,
    pub(crate) locked_until: Option<OffsetDateTime>,
    pub(crate) created_at: OffsetDateTime,
}

impl From<UserRow> for User {
    fn from(row: UserRow) -> Self {
        Self {
            id: UserId::from_uuid(row.id),
            email: row.email,
            password_hash: row.password_hash,
            is_owner: row.is_owner,
            failed_sign_ins: row.failed_sign_ins,
            locked_until: row.locked_until,
            created_at: row.created_at,
        }
    }
}

/// Creates a user. A taken address (compared case-insensitively) or a second owner is a conflict.
pub async fn create_user<'e>(
    executor: impl PgExecutor<'e>,
    email: &Email,
    password_hash: &str,
    is_owner: bool,
) -> Result<User, StoreError> {
    let row: UserRow = sqlx::query_as(concat!(
        "INSERT INTO users (id, email, password_hash, is_owner) VALUES ($1, $2, $3, $4) \
         RETURNING ",
        user_columns!(),
        ""
    ))
    .bind(UserId::new().as_uuid())
    .bind(email.as_str())
    .bind(password_hash)
    .bind(is_owner)
    .fetch_one(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.into())
}

pub async fn find_user<'e>(
    executor: impl PgExecutor<'e>,
    id: UserId,
) -> Result<Option<User>, StoreError> {
    let row: Option<UserRow> = sqlx::query_as(concat!(
        "SELECT ",
        user_columns!(),
        " FROM users WHERE id = $1"
    ))
    .bind(id.as_uuid())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(Into::into))
}

pub async fn find_user_by_email<'e>(
    executor: impl PgExecutor<'e>,
    email: &Email,
) -> Result<Option<User>, StoreError> {
    let row: Option<UserRow> = sqlx::query_as(concat!(
        "SELECT ",
        user_columns!(),
        " FROM users WHERE lower(email) = lower($1)"
    ))
    .bind(email.as_str())
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(Into::into))
}

/// Whether the account is locked, decided with the database clock (the one that stamped
/// `locked_until`). A missing user is not locked.
pub async fn is_locked<'e>(executor: impl PgExecutor<'e>, id: UserId) -> Result<bool, StoreError> {
    let locked: Option<bool> =
        sqlx::query_scalar("SELECT coalesce(locked_until > now(), false) FROM users WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_optional(executor)
            .await
            .map_err(StoreError::query)?;
    Ok(locked.unwrap_or(false))
}

pub async fn list_users<'e>(executor: impl PgExecutor<'e>) -> Result<Vec<User>, StoreError> {
    let rows: Vec<UserRow> = sqlx::query_as(concat!(
        "SELECT ",
        user_columns!(),
        " FROM users ORDER BY created_at, id"
    ))
    .fetch_all(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(rows.into_iter().map(Into::into).collect())
}

pub async fn count_owners<'e>(executor: impl PgExecutor<'e>) -> Result<i64, StoreError> {
    sqlx::query_scalar("SELECT count(*) FROM users WHERE is_owner")
        .fetch_one(executor)
        .await
        .map_err(StoreError::query)
}

/// Replaces the password hash and clears any lockout. Returns whether the user exists.
pub async fn set_password<'e>(
    executor: impl PgExecutor<'e>,
    id: UserId,
    password_hash: &str,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "UPDATE users SET password_hash = $2, failed_sign_ins = 0, locked_until = NULL \
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(password_hash)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(result.rows_affected() == 1)
}

/// Counts a failed sign-in atomically. From the fifth consecutive failure the account is
/// locked for `min(2^(failures - 5) seconds, 15 minutes)`.
pub async fn record_failed_sign_in<'e>(
    executor: impl PgExecutor<'e>,
    id: UserId,
) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE users SET \
            failed_sign_ins = failed_sign_ins + 1, \
            locked_until = CASE WHEN failed_sign_ins + 1 >= $2 \
                THEN now() + make_interval(secs => LEAST( \
                    power(2::float8, LEAST(failed_sign_ins + 1 - $2, 20)::float8), $3)) \
                ELSE locked_until END \
         WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(LOCK_THRESHOLD)
    .bind(MAX_LOCK_SECONDS)
    .execute(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(())
}

/// Resets the failure counter and any lock.
pub async fn record_successful_sign_in<'e>(
    executor: impl PgExecutor<'e>,
    id: UserId,
) -> Result<(), StoreError> {
    sqlx::query("UPDATE users SET failed_sign_ins = 0, locked_until = NULL WHERE id = $1")
        .bind(id.as_uuid())
        .execute(executor)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    fn email(value: &str) -> Email {
        Email::parse(value).unwrap()
    }

    #[test]
    fn debug_output_hides_the_address_and_the_hash() {
        let user = User {
            id: UserId::new(),
            email: "jane@example.com".into(),
            password_hash: "$argon2id$secret".into(),
            is_owner: false,
            failed_sign_ins: 0,
            locked_until: None,
            created_at: OffsetDateTime::now_utc(),
        };

        let debug = format!("{user:?}");

        assert!(!debug.contains("jane"));
        assert!(!debug.contains("argon2"));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_created_user_can_be_found_by_id_and_by_any_casing_of_the_address(pool: PgPool) {
        let user = create_user(&pool, &email("Jane@Example.com"), "hash", false)
            .await
            .unwrap();

        assert_eq!(user.email, "jane@example.com");
        assert_eq!(
            find_user(&pool, user.id).await.unwrap().unwrap().id,
            user.id
        );
        let found = find_user_by_email(&pool, &email("JANE@example.COM"))
            .await
            .unwrap();
        assert_eq!(found.unwrap().id, user.id);
        assert!(
            find_user_by_email(&pool, &email("other@example.com"))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(list_users(&pool).await.unwrap().len(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_duplicate_address_is_a_conflict(pool: PgPool) {
        create_user(&pool, &email("a@example.com"), "hash", false)
            .await
            .unwrap();

        let error = create_user(&pool, &email("A@example.com"), "hash", false)
            .await
            .unwrap_err();

        assert!(error.is_conflict());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_database_allows_only_one_owner(pool: PgPool) {
        create_user(&pool, &email("a@example.com"), "hash", true)
            .await
            .unwrap();

        let error = create_user(&pool, &email("b@example.com"), "hash", true)
            .await
            .unwrap_err();

        assert!(error.is_conflict());
        assert_eq!(count_owners(&pool).await.unwrap(), 1);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn four_failures_do_not_lock_but_the_fifth_does(pool: PgPool) {
        let user = create_user(&pool, &email("a@example.com"), "hash", false)
            .await
            .unwrap();

        for _ in 0..4 {
            record_failed_sign_in(&pool, user.id).await.unwrap();
        }
        let before = find_user(&pool, user.id).await.unwrap().unwrap();
        record_failed_sign_in(&pool, user.id).await.unwrap();
        let after = find_user(&pool, user.id).await.unwrap().unwrap();

        assert_eq!(before.failed_sign_ins, 4);
        assert!(before.locked_until.is_none());
        assert_eq!(after.failed_sign_ins, 5);
        assert!(after.locked_until.is_some());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_lock_grows_exponentially_and_is_capped_at_fifteen_minutes(pool: PgPool) {
        let user = create_user(&pool, &email("a@example.com"), "hash", false)
            .await
            .unwrap();
        let seconds_locked = |pool: PgPool| async move {
            sqlx::query_scalar::<_, f64>(
                "SELECT extract(epoch FROM locked_until - now())::float8 FROM users WHERE id = $1",
            )
            .bind(user.id.as_uuid())
            .fetch_one(&pool)
            .await
            .unwrap()
        };

        for _ in 0..5 {
            record_failed_sign_in(&pool, user.id).await.unwrap();
        }
        let first = seconds_locked(pool.clone()).await;
        for _ in 0..3 {
            record_failed_sign_in(&pool, user.id).await.unwrap();
        }
        let eighth = seconds_locked(pool.clone()).await;
        for _ in 0..40 {
            record_failed_sign_in(&pool, user.id).await.unwrap();
        }
        let many = seconds_locked(pool.clone()).await;

        assert!(first <= 1.0, "{first}");
        assert!((7.0..=8.0).contains(&eighth), "{eighth}");
        assert!((895.0..=900.0).contains(&many), "{many}");
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn an_account_is_locked_only_while_the_database_clock_is_before_locked_until(
        pool: PgPool,
    ) {
        let user = create_user(&pool, &email("a@example.com"), "hash", false)
            .await
            .unwrap();
        assert!(!is_locked(&pool, user.id).await.unwrap());

        for _ in 0..5 {
            record_failed_sign_in(&pool, user.id).await.unwrap();
        }
        assert!(is_locked(&pool, user.id).await.unwrap());

        sqlx::query("UPDATE users SET locked_until = now() - interval '1 second' WHERE id = $1")
            .bind(user.id.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        assert!(!is_locked(&pool, user.id).await.unwrap());
        assert!(!is_locked(&pool, UserId::new()).await.unwrap());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_successful_sign_in_resets_the_counter_and_the_lock(pool: PgPool) {
        let user = create_user(&pool, &email("a@example.com"), "hash", false)
            .await
            .unwrap();
        for _ in 0..6 {
            record_failed_sign_in(&pool, user.id).await.unwrap();
        }

        record_successful_sign_in(&pool, user.id).await.unwrap();

        let user = find_user(&pool, user.id).await.unwrap().unwrap();
        assert_eq!(user.failed_sign_ins, 0);
        assert!(user.locked_until.is_none());
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn setting_a_password_clears_the_lock_and_reports_missing_users(pool: PgPool) {
        let user = create_user(&pool, &email("a@example.com"), "old", false)
            .await
            .unwrap();
        for _ in 0..5 {
            record_failed_sign_in(&pool, user.id).await.unwrap();
        }

        assert!(set_password(&pool, user.id, "new").await.unwrap());
        assert!(!set_password(&pool, UserId::new(), "new").await.unwrap());

        let user = find_user(&pool, user.id).await.unwrap().unwrap();
        assert_eq!(user.password_hash, "new");
        assert_eq!(user.failed_sign_ins, 0);
        assert!(user.locked_until.is_none());
    }
}
