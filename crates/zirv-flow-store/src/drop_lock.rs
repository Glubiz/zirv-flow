//! The lock timeout of a retention partition drop, shared by the event and decision-log
//! retention. A drop takes ACCESS EXCLUSIVE on the parent table, and while it waits for that lock
//! every insert queues behind it; `DETACH ... CONCURRENTLY` is not available with a DEFAULT
//! partition. So a drop waits [`DROP_LOCK_TIMEOUT`] at most and is skipped until the next run.

use sqlx::PgConnection;

use crate::StoreError;

/// How long a partition drop waits for the lock on its parent table before it is skipped.
const DROP_LOCK_TIMEOUT: &str = "2s";

/// SQLSTATE `lock_not_available`: the lock timeout of a drop expired.
const LOCK_NOT_AVAILABLE: &str = "55P03";

/// Bounds the lock waits of the rest of the transaction to [`DROP_LOCK_TIMEOUT`].
pub(crate) async fn set_drop_lock_timeout(connection: &mut PgConnection) -> Result<(), StoreError> {
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(DROP_LOCK_TIMEOUT)
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// Whether `error` is the expiry of the lock timeout set by [`set_drop_lock_timeout`].
pub(crate) fn is_lock_timeout(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|error| error.code())
        .is_some_and(|code| code == LOCK_NOT_AVAILABLE)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_lock_wait_past_the_timeout_is_a_lock_timeout_and_other_errors_are_not(pool: PgPool) {
        let mut holder = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE workspaces IN ACCESS SHARE MODE")
            .execute(&mut *holder)
            .await
            .unwrap();
        let mut waiter = pool.begin().await.unwrap();
        set_drop_lock_timeout(&mut waiter).await.unwrap();

        let timed_out = sqlx::query("LOCK TABLE workspaces IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *waiter)
            .await
            .unwrap_err();
        let other = sqlx::query("SELECT * FROM no_such_table")
            .execute(&pool)
            .await
            .unwrap_err();

        assert!(is_lock_timeout(&timed_out));
        assert!(!is_lock_timeout(&other));
        assert!(!is_lock_timeout(&sqlx::Error::RowNotFound));
    }
}
