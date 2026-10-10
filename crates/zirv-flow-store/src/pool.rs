pub use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::StoreError;

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

        let error = connect(&bad_url, 1).await.unwrap_err().to_string();

        assert!(!error.contains(password));
        assert!(!error.contains(&bad_url));
    }
}
