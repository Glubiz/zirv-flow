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

#[cfg(test)]
mod tests {
    use super::*;

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
