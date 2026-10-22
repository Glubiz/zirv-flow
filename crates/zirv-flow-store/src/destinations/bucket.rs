//! The cross-worker rate bucket of a destination (ruling OQ-9, architecture 10.1 and 14).
//!
//! One row per destination holds the bucket for every worker of every process. It does not
//! replace the per-process `RateLimits` the sources keep.
//!
//! - **Take.** A worker takes `k = clamp(ceil(rate * 0.1 s), 1, 50)` tokens in one
//!   `UPDATE ... RETURNING` that refills the row and grants in the same statement, so concurrent
//!   workers never double-grant. The grant is `floor(min(k, level))`: only what the row holds.
//! - **Refill.** `level = min(burst, tokens + rate * max(0, now - refilled_at))`, with `rate` the
//!   bucket's effective rate at `now` (see below) and `now` the database's clock
//!   (`clock_timestamp()`), never a worker's. `tokens` becomes `level - granted` and
//!   `refilled_at` becomes `now`, so the fraction of a token is kept.
//! - **Lease.** A [`TokenBlock`] is valid for [`LEASE`] (1 s) from the moment its take was
//!   requested. A worker spends from it only while it is valid and discards what is left when it
//!   expires. Unused tokens are never returned to the row.
//! - **429.** [`throttle_bucket`] halves the effective rate (floor 1 per second) and holds it
//!   there until `Retry-After` has passed; a 429 inside that window only extends it. Afterwards
//!   the rate recovers additively, by a tenth of the destination's rate (at least 1) per second,
//!   up to the destination's rate (the SQL function `destination_bucket_rate`). There is no
//!   adaptive block size.
//! - **Bound.** In any window `T` the sends of all workers are at most
//!   `rate * (T + lease) + burst`: a worker may still hold, at the start of the window, a block
//!   granted up to one lease earlier, and the row never grants more than the rate and the burst
//!   allow over the whole period.
//!
//! Tests inject the instant through a [`Clock`]; production passes `Clock::Database`.

use std::time::{Duration, Instant};

use sqlx::PgExecutor;
use sqlx::prelude::FromRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{Clock, StoreError};

/// How long a granted block of tokens stays valid.
pub const LEASE: Duration = Duration::from_secs(1);
/// The most tokens one take grants.
pub const MAX_BLOCK: u32 = 50;
/// The penalty window of a 429 that carries no `Retry-After`.
pub const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(1);
/// The longest penalty window a 429 may set, however large its `Retry-After` is.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

/// The instant to pass to SQL: none for the database's own clock.
fn instant_of(clock: &Clock) -> Option<OffsetDateTime> {
    match clock {
        Clock::Database => None,
        Clock::Manual(manual) => Some(manual.now()),
    }
}

/// What one take granted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Grant {
    /// Whole tokens granted; 0 when the row held less than one.
    pub tokens: u32,
    /// With no token granted, how long until the row holds one at its current rate.
    pub retry_in: Duration,
}

#[derive(FromRow)]
struct GrantRow {
    granted: i32,
    wait_ms: i64,
}

/// Takes a block of tokens from a destination's bucket, refilling it first, in one statement.
/// `None` when the destination has no bucket. `clock` is the database's unless a test injects
/// time.
pub async fn take_tokens<'e>(
    executor: impl PgExecutor<'e>,
    destination: Uuid,
    clock: &Clock,
) -> Result<Option<Grant>, StoreError> {
    let row: Option<GrantRow> = sqlx::query_as(
        "WITH cur AS ( \
            SELECT b.destination_id, d.rate::float8 AS cap, d.burst AS burst, b.rate AS stored, \
                   b.tokens, b.refilled_at, b.penalty_until, \
                   COALESCE($2::timestamptz, clock_timestamp()) AS ts \
            FROM destination_rate_buckets b \
            JOIN destinations d ON d.workspace_id = b.workspace_id AND d.id = b.destination_id \
            WHERE b.destination_id = $1 \
            FOR UPDATE OF b), \
         rated AS ( \
            SELECT cur.*, destination_bucket_rate(stored, cap, penalty_until, ts) AS rate \
            FROM cur), \
         filled AS ( \
            SELECT rated.*, LEAST(burst::float8, \
                       tokens + rate * GREATEST(0, EXTRACT(EPOCH FROM (ts - refilled_at)))) AS level \
            FROM rated), \
         granted AS ( \
            SELECT filled.*, FLOOR(LEAST(level, \
                       LEAST(50, GREATEST(1, CEIL(rate * 0.1))))) AS granted \
            FROM filled) \
         UPDATE destination_rate_buckets b SET \
            rate = CASE WHEN g.penalty_until IS NOT NULL AND g.ts > g.penalty_until \
                             AND g.rate >= g.cap THEN g.cap ELSE g.stored END, \
            penalty_until = CASE WHEN g.penalty_until IS NOT NULL AND g.ts > g.penalty_until \
                                      AND g.rate >= g.cap THEN NULL ELSE g.penalty_until END, \
            burst = g.burst, tokens = g.level - g.granted, refilled_at = g.ts \
         FROM granted g WHERE b.destination_id = g.destination_id \
         RETURNING g.granted::int AS granted, \
                   (CASE WHEN g.granted >= 1 THEN 0 \
                         ELSE CEIL((1 - g.level) / g.rate * 1000) END)::bigint AS wait_ms",
    )
    .bind(destination)
    .bind(instant_of(clock))
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)?;
    Ok(row.map(|row| Grant {
        tokens: u32::try_from(row.granted).unwrap_or(0),
        retry_in: Duration::from_millis(u64::try_from(row.wait_ms).unwrap_or(0)),
    }))
}

/// Records a 429 from the destination: halves the effective rate (floor 1 per second) and holds
/// it until `retry_after` has passed (1 s without one, at most an hour), after which it recovers
/// additively. A 429 that arrives while a window is open only extends the window, so a burst of
/// them from concurrent workers halves the rate once. Returns the state afterwards; `None` when
/// the destination has no bucket.
pub async fn throttle_bucket<'e>(
    executor: impl PgExecutor<'e>,
    destination: Uuid,
    retry_after: Option<Duration>,
    clock: &Clock,
) -> Result<Option<BucketState>, StoreError> {
    let pause = retry_after
        .unwrap_or(DEFAULT_RETRY_AFTER)
        .min(MAX_RETRY_AFTER)
        .as_secs_f64();
    sqlx::query_as(
        "WITH cur AS ( \
            SELECT b.destination_id, d.rate::float8 AS cap, b.rate AS stored, b.penalty_until, \
                   COALESCE($2::timestamptz, clock_timestamp()) AS ts \
            FROM destination_rate_buckets b \
            JOIN destinations d ON d.workspace_id = b.workspace_id AND d.id = b.destination_id \
            WHERE b.destination_id = $1 \
            FOR UPDATE OF b) \
         UPDATE destination_rate_buckets b SET \
            rate = CASE WHEN c.penalty_until IS NOT NULL AND c.ts <= c.penalty_until THEN b.rate \
                        ELSE GREATEST(1, destination_bucket_rate(c.stored, c.cap, c.penalty_until, \
                                                                 c.ts) / 2) END, \
            penalty_until = GREATEST( \
                CASE WHEN c.penalty_until > c.ts THEN c.penalty_until ELSE c.ts END, \
                c.ts + make_interval(secs => $3::float8)) \
         FROM cur c WHERE b.destination_id = c.destination_id \
         RETURNING b.rate, b.burst, b.tokens, b.refilled_at, b.penalty_until",
    )
    .bind(destination)
    .bind(instant_of(clock))
    .bind(pause)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// A bucket row as stored.
#[derive(Clone, Debug, FromRow, PartialEq)]
pub struct BucketState {
    /// The stored rate: the halved rate while a penalty or its recovery lasts, else the
    /// destination's.
    pub rate: f64,
    pub burst: i32,
    pub tokens: f64,
    pub refilled_at: OffsetDateTime,
    pub penalty_until: Option<OffsetDateTime>,
}

/// The bucket row of a destination.
pub async fn bucket_state<'e>(
    executor: impl PgExecutor<'e>,
    destination: Uuid,
) -> Result<Option<BucketState>, StoreError> {
    sqlx::query_as(
        "SELECT rate, burst, tokens, refilled_at, penalty_until \
         FROM destination_rate_buckets WHERE destination_id = $1",
    )
    .bind(destination)
    .fetch_optional(executor)
    .await
    .map_err(StoreError::query)
}

/// Tokens a worker holds under a lease. Tokens that outlive the lease are discarded, never spent
/// and never returned to the bucket.
#[derive(Clone, Copy, Debug)]
pub struct TokenBlock {
    remaining: u32,
    expires: Instant,
}

impl TokenBlock {
    /// A block of `tokens`, valid for [`LEASE`] after `requested_at`: the moment the take was
    /// requested, which is never later than the moment the row granted it, so a worker never
    /// outlives the lease the row accounted for.
    pub fn new(tokens: u32, requested_at: Instant) -> Self {
        Self {
            remaining: tokens,
            expires: requested_at + LEASE,
        }
    }

    /// An empty block, as a worker starts with.
    pub fn empty(now: Instant) -> Self {
        Self::new(0, now)
    }

    /// Spends one token if the block is still valid and holds one. An expired block is emptied.
    pub fn spend(&mut self, now: Instant) -> bool {
        if now >= self.expires {
            self.remaining = 0;
        }
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }

    /// Tokens left that could still be spent at `now`.
    pub fn remaining(&self, now: Instant) -> u32 {
        if now >= self.expires {
            0
        } else {
            self.remaining
        }
    }
}

/// The answer to a worker asking to send.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    /// One token was spent: send now.
    Granted,
    /// The bucket is empty: ask again after this long.
    Wait(Duration),
    /// The destination has no bucket.
    Gone,
}

/// One worker's view of a destination's bucket: the block it holds. Not shared between workers;
/// each worker owns one per destination it sends to.
#[derive(Debug)]
pub struct RateGate {
    destination: Uuid,
    block: TokenBlock,
}

impl RateGate {
    pub fn new(destination: Uuid) -> Self {
        Self {
            destination,
            block: TokenBlock::empty(Instant::now()),
        }
    }

    /// Spends a token from the held block while it is valid; otherwise discards the block and
    /// takes a new one from the row.
    pub async fn admit<'e>(
        &mut self,
        executor: impl PgExecutor<'e>,
        clock: &Clock,
    ) -> Result<Admission, StoreError> {
        if self.block.spend(Instant::now()) {
            return Ok(Admission::Granted);
        }
        let requested = Instant::now();
        let Some(grant) = take_tokens(executor, self.destination, clock).await? else {
            return Ok(Admission::Gone);
        };
        self.block = TokenBlock::new(grant.tokens, requested);
        if self.block.spend(Instant::now()) {
            Ok(Admission::Granted)
        } else {
            Ok(Admission::Wait(grant.retry_in))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;
    use sqlx::PgPool;
    use time::macros::datetime;
    use zirv_flow_core::{Slug, WorkspaceId, WorkspaceName};

    use super::*;
    use crate::{ManualClock, NewDestination, create_destination, create_workspace};

    const T0: OffsetDateTime = datetime!(2030-01-01 00:00:00 UTC);

    async fn workspace(pool: &PgPool, slug: &str) -> WorkspaceId {
        let name = WorkspaceName::parse(slug).unwrap();
        let slug = Slug::parse(slug).unwrap();
        create_workspace(pool, &name, &slug).await.unwrap().id
    }

    /// A destination whose bucket is full at `T0`, so a manual clock can start there.
    async fn destination(pool: &PgPool, rate: i32, burst: i32) -> Uuid {
        let ws = workspace(pool, "one").await;
        destination_in(pool, ws, "Mail", rate, burst).await
    }

    async fn destination_in(
        pool: &PgPool,
        ws: WorkspaceId,
        name: &str,
        rate: i32,
        burst: i32,
    ) -> Uuid {
        let config = json!({});
        let id = create_destination(
            pool,
            ws,
            &NewDestination {
                id: Uuid::now_v7(),
                kind: "fake",
                name,
                config: &config,
                credential_secret_id: None,
                credential_fields: &[],
                rate,
                burst,
                created_by: None,
            },
        )
        .await
        .unwrap()
        .id;
        sqlx::query(
            "UPDATE destination_rate_buckets SET refilled_at = $2, tokens = burst WHERE destination_id = $1",
        )
        .bind(id)
        .bind(T0)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    fn at(clock: &ManualClock) -> Clock {
        Clock::Manual(clock.clone())
    }

    async fn grant(pool: &PgPool, id: Uuid, clock: &Clock) -> Grant {
        take_tokens(pool, id, clock).await.unwrap().unwrap()
    }

    // ---- the block a worker holds ----

    #[test]
    fn a_block_is_spent_down_and_an_expired_block_is_discarded_and_never_reused() {
        let start = Instant::now();
        let mut block = TokenBlock::new(3, start);

        assert!(block.spend(start));
        assert_eq!(block.remaining(start), 2);
        // The lease ends 1 s after the request: the two tokens left are discarded.
        let late = start + LEASE;
        assert_eq!(block.remaining(late), 0);
        assert!(!block.spend(late));
        // Not even the clock going back brings them back.
        assert!(!block.spend(start));
        assert_eq!(block.remaining(start), 0);
    }

    #[test]
    fn a_token_is_spendable_until_the_instant_before_the_lease_ends() {
        let start = Instant::now();
        let mut block = TokenBlock::new(1, start);

        assert!(block.spend(start + LEASE - Duration::from_millis(1)));
        assert!(!block.spend(start));
        assert!(!TokenBlock::empty(start).spend(start));
    }

    // ---- the take ----

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_take_grants_a_tenth_of_a_second_of_rate_clamped_and_only_what_the_row_holds(
        pool: PgPool,
    ) {
        let manual = ManualClock::new(T0);
        let clock = at(&manual);
        let ws = workspace(&pool, "one").await;
        // k = ceil(5 * 0.1) = 1.
        let slow = destination_in(&pool, ws, "Slow", 5, 5).await;
        assert_eq!(grant(&pool, slow, &clock).await.tokens, 1);
        // k = ceil(100 * 0.1) = 10, then the 7 tokens left (burst 17).
        let mid = destination_in(&pool, ws, "Mid", 100, 17).await;
        assert_eq!(grant(&pool, mid, &clock).await.tokens, 10);
        assert_eq!(grant(&pool, mid, &clock).await.tokens, 7);
        let empty = grant(&pool, mid, &clock).await;
        assert_eq!(empty.tokens, 0);
        // One token at 100/s takes 10 ms.
        assert_eq!(empty.retry_in, Duration::from_millis(10));
        // k is capped at 50 however high the rate is.
        let fast = destination_in(&pool, ws, "Fast", 100_000, 100_000).await;
        assert_eq!(grant(&pool, fast, &clock).await.tokens, MAX_BLOCK);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_row_refills_with_time_up_to_the_burst_and_keeps_the_fraction(pool: PgPool) {
        let id = destination(&pool, 100, 20).await;
        let manual = ManualClock::new(T0);
        let clock = at(&manual);
        // Drain: 10 + 10.
        assert_eq!(grant(&pool, id, &clock).await.tokens, 10);
        assert_eq!(grant(&pool, id, &clock).await.tokens, 10);
        assert_eq!(grant(&pool, id, &clock).await.tokens, 0);

        // 55 ms at 100/s is 5.5 tokens: five are granted, half a token stays.
        manual.advance(time::Duration::milliseconds(55));
        assert_eq!(grant(&pool, id, &clock).await.tokens, 5);
        let state = bucket_state(&pool, id).await.unwrap().unwrap();
        assert!((state.tokens - 0.5).abs() < 1e-6, "{}", state.tokens);
        assert_eq!(state.refilled_at, T0 + time::Duration::milliseconds(55));

        // An hour later the row holds the burst and no more.
        manual.advance(time::Duration::hours(1));
        assert_eq!(grant(&pool, id, &clock).await.tokens, 10);
        let state = bucket_state(&pool, id).await.unwrap().unwrap();
        assert!((state.tokens - 10.0).abs() < 1e-6, "{}", state.tokens);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn tokens_granted_are_gone_from_the_row_and_never_returned(pool: PgPool) {
        let id = destination(&pool, 100, 100).await;
        let manual = ManualClock::new(T0);
        let clock = at(&manual);
        let mut gate = RateGate::new(id);

        // One admit takes a block of 10 and spends 1; the other 9 are the worker's to use or
        // lose.
        assert_eq!(gate.admit(&pool, &clock).await.unwrap(), Admission::Granted);
        let held = bucket_state(&pool, id).await.unwrap().unwrap();
        assert!((held.tokens - 90.0).abs() < 1e-6, "{}", held.tokens);

        // Discarding the block (letting its lease end) does not put the tokens back.
        gate.block = TokenBlock::empty(Instant::now());
        let after = bucket_state(&pool, id).await.unwrap().unwrap();
        assert!((after.tokens - 90.0).abs() < 1e-6, "{}", after.tokens);
        assert_eq!(gate.admit(&pool, &clock).await.unwrap(), Admission::Granted);
        let next = bucket_state(&pool, id).await.unwrap().unwrap();
        assert!((next.tokens - 80.0).abs() < 1e-6, "{}", next.tokens);
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_gate_waits_when_the_row_is_empty_and_reports_a_missing_bucket(pool: PgPool) {
        let id = destination(&pool, 100, 10).await;
        let manual = ManualClock::new(T0);
        let clock = at(&manual);
        let mut gate = RateGate::new(id);

        for _ in 0..10 {
            assert_eq!(gate.admit(&pool, &clock).await.unwrap(), Admission::Granted);
        }
        assert_eq!(
            gate.admit(&pool, &clock).await.unwrap(),
            Admission::Wait(Duration::from_millis(10))
        );
        assert_eq!(
            RateGate::new(Uuid::now_v7())
                .admit(&pool, &clock)
                .await
                .unwrap(),
            Admission::Gone
        );
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn concurrent_takes_at_one_instant_never_grant_more_than_the_row_holds(pool: PgPool) {
        let id = destination(&pool, 1000, 100).await;
        let clock = at(&ManualClock::new(T0));
        let granted = Arc::new(AtomicU64::new(0));

        let mut tasks = Vec::new();
        for _ in 0..16 {
            let (pool, clock, granted) = (pool.clone(), clock.clone(), Arc::clone(&granted));
            tasks.push(tokio::spawn(async move {
                for _ in 0..8 {
                    let got = take_tokens(&pool, id, &clock).await.unwrap().unwrap();
                    granted.fetch_add(u64::from(got.tokens), Ordering::SeqCst);
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        // 128 takes asked for up to 50 each; the row held exactly its burst.
        assert_eq!(granted.load(Ordering::SeqCst), 100);
        let state = bucket_state(&pool, id).await.unwrap().unwrap();
        assert!(state.tokens.abs() < 1e-6, "{}", state.tokens);
    }

    // ---- 429 ----

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn a_429_halves_the_rate_until_retry_after_and_then_the_rate_recovers_additively(
        pool: PgPool,
    ) {
        let id = destination(&pool, 100, 100).await;
        let manual = ManualClock::new(T0);
        let clock = at(&manual);
        // Drain the bucket so only the refill rate shows.
        for _ in 0..10 {
            grant(&pool, id, &clock).await;
        }

        // The 429 halves 100/s to 50/s for 10 s.
        let state = throttle_bucket(&pool, id, Some(Duration::from_secs(10)), &clock)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.rate, 50.0);
        assert_eq!(state.penalty_until, Some(T0 + time::Duration::seconds(10)));
        // Inside the window the row refills at 50/s: 1 s gives 50 tokens, not 100.
        manual.advance(time::Duration::seconds(1));
        let mut total = 0;
        loop {
            let got = grant(&pool, id, &clock).await.tokens;
            if got == 0 {
                break;
            }
            total += got;
        }
        assert_eq!(total, 50);

        // A second 429 inside the window extends it and halves nothing.
        manual.advance(time::Duration::seconds(4));
        let again = throttle_bucket(&pool, id, Some(Duration::from_secs(10)), &clock)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.rate, 50.0);
        assert_eq!(again.penalty_until, Some(T0 + time::Duration::seconds(15)));

        // After the window the rate climbs by 10/s per second (a tenth of 100/s): 2 s after the
        // window ends it is 70/s.
        manual.set(T0 + time::Duration::seconds(17));
        grant(&pool, id, &clock).await;
        let mid = bucket_state(&pool, id).await.unwrap().unwrap();
        assert_eq!(
            mid.rate, 50.0,
            "the stored rate is left alone during recovery"
        );
        assert!(mid.penalty_until.is_some());
        let recovering = effective_rate(&pool, id, &clock).await;
        assert!((recovering - 70.0).abs() < 1e-6, "{recovering}");

        // Fully recovered after 5 s: the penalty clears and the rate is the destination's.
        manual.set(T0 + time::Duration::seconds(21));
        grant(&pool, id, &clock).await;
        let done = bucket_state(&pool, id).await.unwrap().unwrap();
        assert_eq!(done.rate, 100.0);
        assert!(done.penalty_until.is_none());
    }

    async fn effective_rate(pool: &PgPool, id: Uuid, clock: &Clock) -> f64 {
        sqlx::query_scalar(
            "SELECT destination_bucket_rate(b.rate, d.rate::float8, b.penalty_until, $2) \
             FROM destination_rate_buckets b JOIN destinations d ON d.id = b.destination_id \
             WHERE b.destination_id = $1",
        )
        .bind(id)
        .bind(instant_of(clock))
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn the_halved_rate_never_goes_below_one_per_second_and_a_missing_retry_after_is_one_second(
        pool: PgPool,
    ) {
        let id = destination(&pool, 3, 3).await;
        let manual = ManualClock::new(T0);
        let clock = at(&manual);

        let first = throttle_bucket(&pool, id, None, &clock)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.rate, 1.5);
        assert_eq!(first.penalty_until, Some(T0 + time::Duration::seconds(1)));
        // Once the window has passed the recovery has not reached 3/s yet (step 1/s, 0.5 s in),
        // and a new 429 halves the recovered 2/s, not the stored 1.5/s.
        manual.set(T0 + time::Duration::milliseconds(1500));
        let second = throttle_bucket(&pool, id, None, &clock)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.rate, 1.0);
        manual.set(T0 + time::Duration::seconds(30));
        let third = throttle_bucket(&pool, id, Some(Duration::from_secs(99_999)), &clock)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(third.rate, 1.5);
        assert_eq!(
            third.penalty_until,
            Some(T0 + time::Duration::seconds(30) + time::Duration::hours(1)),
            "the window is capped at an hour"
        );
        assert!(
            throttle_bucket(&pool, Uuid::now_v7(), None, &clock)
                .await
                .unwrap()
                .is_none()
        );
    }

    // ---- exit criterion 7 ----

    /// N workers of separate gates draw tokens from one row for a window T on the database's
    /// clock. The sends of all of them stay within `rate * (T + lease) + burst`.
    ///
    /// N = 8 workers, rate 100/s, burst 10 (blocks of 10), T = 3 s, lease 1 s:
    /// bound = 100 * (3 + 1) + 10 = 410. The floor, 70% of `rate * T` = 210, is the tolerance
    /// for scheduling and database latency on a busy machine: a bucket that granted nothing
    /// would pass the bound alone, so the floor shows the workers did draw.
    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn n_concurrent_workers_stay_within_the_destination_rate_over_a_window(pool: PgPool) {
        const WORKERS: usize = 8;
        const RATE: u64 = 100;
        const BURST: u64 = 10;
        const WINDOW: Duration = Duration::from_secs(3);
        let id = destination(&pool, RATE as i32, BURST as i32).await;
        // Start from the database's clock, with a full bucket.
        sqlx::query(
            "UPDATE destination_rate_buckets SET refilled_at = clock_timestamp(), tokens = burst WHERE destination_id = $1",
        )
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();

        let sent = Arc::new(AtomicU64::new(0));
        let started = Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..WORKERS {
            let (pool, sent) = (pool.clone(), Arc::clone(&sent));
            tasks.push(tokio::spawn(async move {
                let mut gate = RateGate::new(id);
                while started.elapsed() < WINDOW {
                    match gate.admit(&pool, &Clock::Database).await.unwrap() {
                        Admission::Granted => {
                            sent.fetch_add(1, Ordering::SeqCst);
                        }
                        Admission::Wait(pause) => {
                            tokio::time::sleep(pause.max(Duration::from_millis(5))).await;
                        }
                        Admission::Gone => panic!("the bucket exists"),
                    }
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        let total = sent.load(Ordering::SeqCst);
        let bound = RATE * (WINDOW.as_secs() + LEASE.as_secs()) + BURST;
        let floor = RATE * WINDOW.as_secs() * 7 / 10;
        assert_eq!(bound, 410);
        assert!(total <= bound, "{total} sends exceed the bound {bound}");
        assert!(total >= floor, "{total} sends are below the floor {floor}");
        eprintln!("criterion 7: N={WORKERS} T={WINDOW:?} bound={bound} observed={total}");
    }
}
