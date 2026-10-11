use std::time::Duration;

pub use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::StoreError;

/// An open database transaction; dropping it without committing rolls back.
pub type Tx = sqlx::Transaction<'static, sqlx::Postgres>;

/// Starts a transaction.
pub async fn begin(pool: &PgPool) -> Result<Tx, StoreError> {
    pool.begin().await.map_err(StoreError::query)
}

/// Commits a transaction.
pub async fn commit(tx: Tx) -> Result<(), StoreError> {
    tx.commit().await.map_err(StoreError::query)
}

pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool, StoreError> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(url)
        .await
        .map_err(StoreError::connect)
}

/// A separate pool to the same database, with its own `max_connections`,
/// `min_connections` and `acquire_timeout`. It connects lazily, so building it cannot
/// fail; `min_connections` are opened in the background and kept warm, so an acquire
/// need not pay for a new connection. What it holds never counts against `pool`.
pub fn sized_like(
    pool: &PgPool,
    max_connections: u32,
    min_connections: u32,
    acquire_timeout: Duration,
) -> PgPool {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .min_connections(min_connections)
        .acquire_timeout(acquire_timeout)
        .connect_lazy_with((*pool.connect_options()).clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_sized_pool_is_independent_of_the_pool_it_copies(pool: PgPool) {
        let mut held = Vec::new();
        for _ in 0..pool.options().get_max_connections() {
            held.push(pool.acquire().await.unwrap());
        }
        let sibling = sized_like(&pool, 2, 1, Duration::from_secs(5));

        // Every connection of the original is held; the sibling still answers.
        let one: i32 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&sibling)
            .await
            .unwrap();

        assert_eq!(one, 1);
        drop(held);
        assert_eq!(sibling.options().get_max_connections(), 2);
        assert_eq!(sibling.options().get_min_connections(), 1);
        assert_eq!(
            sibling.options().get_acquire_timeout(),
            Duration::from_secs(5)
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_connection_failure_does_not_echo_the_password(_pool: PgPool) {
        let url = std::env::var("DATABASE_URL").unwrap();
        let (scheme, remainder) = url.split_once("://").unwrap();
        let (credentials, address) = remainder.rsplit_once('@').unwrap();
        let username = credentials
            .split_once(':')
            .map_or(credentials, |(name, _)| name);
        let password = "wrong-password-token";
        let bad_url = format!("{scheme}://{username}:{password}@{address}");

        let error = connect(&bad_url, 1).await.unwrap_err();

        let display = error.to_string();
        assert!(
            display.contains("password authentication failed"),
            "{display}"
        );
        let mut current: Option<&dyn std::error::Error> = Some(&error);
        while let Some(link) = current {
            let text = link.to_string();
            assert!(!text.contains(password), "{text}");
            assert!(!text.contains(&bad_url), "{text}");
            current = link.source();
        }
    }
}
