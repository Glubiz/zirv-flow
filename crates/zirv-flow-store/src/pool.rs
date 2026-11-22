use std::time::Duration;

pub use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::StoreError;

/// An open database transaction; dropping it without committing rolls back.
pub type Tx = sqlx::Transaction<'static, sqlx::Postgres>;

/// Starts a transaction.
///
/// Cancel safe: a caller dropped while `BEGIN` is in flight leaves no open transaction behind.
/// sqlx queues a rollback only once its transaction depth is above zero, so a `begin` cancelled
/// after `BEGIN` was sent has the server open a transaction that the connection returns to the
/// pool in, and its next user runs inside it (an aborted `ingest.drain` left such a backend
/// `idle in transaction`, #855). The connection is taken in the caller's own future, where
/// cancelling it only gives it back; `BEGIN` runs in a task that finishes it either way, and a
/// transaction nobody receives is dropped there, which rolls it back.
pub async fn begin(pool: &PgPool) -> Result<Tx, StoreError> {
    let connection = pool.acquire().await.map_err(StoreError::query)?;
    tokio::spawn(sqlx::Transaction::begin(connection, None))
        .await
        .map_err(|error| match error.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(_) => StoreError::query(sqlx::Error::PoolClosed),
        })?
        .map_err(StoreError::query)
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
/// `min_connections` and `acquire_timeout`. Building it cannot fail. What it holds never
/// counts against `pool`.
///
/// With `min_connections` above zero the pool is warm: those connections open in the
/// background straight away and are kept, so an acquire need not pay for a new one.
/// sqlx opens them at start only when no idle or lifetime reaper runs, so a warm pool
/// has neither.
pub fn sized_like(
    pool: &PgPool,
    max_connections: u32,
    min_connections: u32,
    acquire_timeout: Duration,
) -> PgPool {
    let mut options = PgPoolOptions::new()
        .max_connections(max_connections)
        .min_connections(min_connections)
        .acquire_timeout(acquire_timeout);
    if min_connections > 0 {
        options = options.idle_timeout(None).max_lifetime(None);
    }
    options.connect_lazy_with((*pool.connect_options()).clone())
}

/// Sets the pool gauges through the `metrics` facade. `name` labels the pool.
pub fn observe_pool(pool: &PgPool, name: &'static str) {
    let size = pool.size();
    let idle = u32::try_from(pool.num_idle()).unwrap_or(u32::MAX).min(size);
    metrics::gauge!("zirv_flow_db_pool_connections", "pool" => name, "state" => "idle")
        .set(f64::from(idle));
    metrics::gauge!("zirv_flow_db_pool_connections", "pool" => name, "state" => "in_use")
        .set(f64::from(size - idle));
    metrics::gauge!("zirv_flow_db_pool_max_connections", "pool" => name)
        .set(f64::from(pool.options().get_max_connections()));
}

#[cfg(test)]
mod tests {
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;

    /// A connection that ran `begin` and was cancelled in flight must not return to the pool inside
    /// a transaction the server opened for it (#855): the next user of the connection would run in
    /// a transaction nobody commits, and its backend would stay `idle in transaction`.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_begin_cancelled_in_flight_leaves_no_open_transaction_in_the_pool(pool: PgPool) {
        for round in 0..600_u32 {
            let task = tokio::spawn({
                let pool = pool.clone();
                async move { begin(&pool).await.map(drop) }
            });
            // Cancelled after a varying number of turns of the runtime, so that some rounds land
            // while `BEGIN` is on its way.
            for _ in 0..round % 9 {
                tokio::task::yield_now().await;
            }
            task.abort();
            let _ = task.await;
            // Every connection back, and none inside a transaction: outside one, `SAVEPOINT` fails
            // with no_active_sql_transaction (25P01); inside one it succeeds, or fails with 25P02
            // when that transaction is aborted.
            let mut held = Vec::new();
            while held.len() < pool.size() as usize {
                held.push(pool.acquire().await.unwrap());
            }
            for mut connection in held {
                let outside = sqlx::raw_sql("SAVEPOINT leak_probe")
                    .execute(&mut *connection)
                    .await
                    .err()
                    .and_then(|error| {
                        error
                            .as_database_error()?
                            .code()
                            .map(|code| code == "25P01")
                    })
                    .unwrap_or(false);
                assert!(
                    outside,
                    "a connection came back inside a transaction after a cancelled begin (round {round})"
                );
            }
        }
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn pool_gauges_split_idle_from_in_use_and_report_the_maximum(pool: PgPool) {
        let held = pool.acquire().await.unwrap();
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || observe_pool(&pool, "main"));
        let output = handle.render();

        assert!(
            output.contains("zirv_flow_db_pool_connections{pool=\"main\",state=\"in_use\"} 1"),
            "{output}"
        );
        assert!(
            output.contains("zirv_flow_db_pool_connections{pool=\"main\",state=\"idle\"}"),
            "{output}"
        );
        let max = pool.options().get_max_connections();
        assert!(
            output.contains(&format!(
                "zirv_flow_db_pool_max_connections{{pool=\"main\"}} {max}"
            )),
            "{output}"
        );
        drop(held);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_warm_pool_opens_its_minimum_without_being_asked(pool: PgPool) {
        let warm = sized_like(&pool, 3, 2, Duration::from_secs(5));
        let cold = sized_like(&pool, 3, 0, Duration::from_secs(5));

        let opened = tokio::time::timeout(Duration::from_secs(60), async {
            while warm.size() < 2 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;

        assert!(opened.is_ok(), "the warm pool never opened its connections");
        assert_eq!(cold.size(), 0);
    }

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
