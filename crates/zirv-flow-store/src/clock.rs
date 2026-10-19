//! The clock segment paths read (ruling 5292ff7e). Production reads the database's clock, the
//! only one every instance agrees on; a test uses a [`ManualClock`] it moves, so a relative
//! window such as "no order in 30 days" can be crossed without waiting and without new data.
//!
//! It is an enum rather than a trait object so nothing needs `async_trait`. Only the segment
//! paths adopt it: the segment refresh in the worker and the preview in the API.

use std::sync::{Arc, Mutex, PoisonError};

use sqlx::PgConnection;
use time::{Duration, OffsetDateTime};

use crate::StoreError;

/// An instant held in memory that a test sets and advances. Clones share the instant.
#[derive(Clone, Debug)]
pub struct ManualClock {
    now: Arc<Mutex<OffsetDateTime>>,
}

impl ManualClock {
    pub fn new(start: OffsetDateTime) -> Self {
        Self {
            now: Arc::new(Mutex::new(start)),
        }
    }

    pub fn now(&self) -> OffsetDateTime {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Moves the clock to `instant`, forward or back.
    pub fn set(&self, instant: OffsetDateTime) {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner) = instant;
    }

    /// Moves the clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock().unwrap_or_else(PoisonError::into_inner);
        *now += by;
    }
}

/// Where "now" comes from.
#[derive(Clone, Debug, Default)]
pub enum Clock {
    /// The database's `clock_timestamp()`.
    #[default]
    Database,
    /// A test's instant.
    Manual(ManualClock),
}

impl Clock {
    /// The current instant. The database variant reads `clock_timestamp()`, the wall time at the
    /// call and not the transaction's start, so a refresh that takes it after waiting for the
    /// segment lock gets a time no earlier than the refresh it waited for committed. The manual
    /// variant ignores `connection`.
    /// A read cancelled at the connection's statement timeout is [`StoreError::is_timeout`].
    pub async fn now(&self, connection: &mut PgConnection) -> Result<OffsetDateTime, StoreError> {
        match self {
            Self::Database => sqlx::query_scalar("SELECT clock_timestamp()")
                .fetch_one(connection)
                .await
                .map_err(StoreError::bounded),
            Self::Manual(clock) => Ok(clock.now()),
        }
    }

    /// The current instant for a path that checks a request against the process's wall time and
    /// reads no database: the wall clock for [`Clock::Database`], the test's instant for
    /// [`Clock::Manual`].
    pub fn now_in_process(&self) -> OffsetDateTime {
        match self {
            Self::Database => OffsetDateTime::now_utc(),
            Self::Manual(clock) => clock.now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use time::macros::datetime;

    use super::*;

    #[test]
    fn a_manual_clock_is_shared_by_its_clones_and_moves_only_when_told() {
        let clock = ManualClock::new(datetime!(2030-01-01 00:00 UTC));
        let copy = clock.clone();

        copy.advance(Duration::days(11));
        assert_eq!(clock.now(), datetime!(2030-01-12 00:00 UTC));
        clock.set(datetime!(2029-12-31 00:00 UTC));
        assert_eq!(copy.now(), datetime!(2029-12-31 00:00 UTC));
        assert!(matches!(Clock::default(), Clock::Database));
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_database_clock_is_the_wall_time_not_the_transaction_start(pool: PgPool) {
        let mut tx = pool.begin().await.unwrap();
        let started: OffsetDateTime = sqlx::query_scalar("SELECT now()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let read = Clock::Database.now(&mut tx).await.unwrap();
        let still: OffsetDateTime = sqlx::query_scalar("SELECT now()")
            .fetch_one(&mut *tx)
            .await
            .unwrap();

        assert_eq!(still, started);
        assert!(
            read - started >= Duration::milliseconds(100),
            "{read} {started}"
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_manual_variant_ignores_the_database_and_the_database_variant_reads_it(
        pool: PgPool,
    ) {
        let mut connection = pool.acquire().await.unwrap();
        let start = datetime!(2030-01-01 00:00 UTC);
        let manual = Clock::Manual(ManualClock::new(start));

        assert_eq!(manual.now(&mut connection).await.unwrap(), start);
        let database = Clock::Database.now(&mut connection).await.unwrap();
        assert!((OffsetDateTime::now_utc() - database).abs() < Duration::minutes(1));
    }
}
