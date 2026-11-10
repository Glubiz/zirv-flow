//! Keeping a transaction within the 64 subtransaction ids a PostgreSQL 16 backend caches
//! (Investigation Q, step 4).
//!
//! A savepoint takes a subtransaction id once it writes (a nested one gives its parent one too),
//! and a released one keeps it until the transaction ends; one rolled back gives it up. Past 64
//! the backend is marked overflowed, and every snapshot check in every backend has to read
//! `pg_subtrans` until the transaction ends, which showed as 13 to 24 backends waiting on
//! `LWLock:SubtransSLRU`. So a unit of work that needs to keep or refuse each of its items alone
//! applies them through [`apply_isolated`] instead of a savepoint each.
//!
//! A rollback gives its id back only when the savepoint goes with it. `ROLLBACK TO SAVEPOINT`,
//! what a nested `sqlx` transaction's rollback runs, leaves the savepoint in place: the next write
//! inside it (and every later savepoint begins inside it) takes an id again, kept to the end of
//! the enclosing transaction. [`roll_back_savepoint`] releases the savepoint after rolling back.

use sqlx::PgConnection;

use crate::StoreError;

/// The most runs the [`apply_isolated`] calls of one transaction release together: a few under
/// the 64 ids a backend caches, for what the transaction takes besides.
pub(crate) const MAX_RUNS: usize = 60;

/// Begins the savepoint `name`, a fixed name.
pub(crate) async fn begin_savepoint(
    connection: &mut PgConnection,
    name: &'static str,
) -> Result<(), StoreError> {
    savepoint_statement(connection, "SAVEPOINT", name).await
}

/// Releases the savepoint `name`: its writes stay, and so does the id it took, to the end of the
/// enclosing transaction.
pub(crate) async fn release_savepoint(
    connection: &mut PgConnection,
    name: &'static str,
) -> Result<(), StoreError> {
    savepoint_statement(connection, "RELEASE SAVEPOINT", name).await
}

/// Rolls back to the savepoint `name` and releases it, which drops its writes and its locks and
/// gives back the id it took.
pub(crate) async fn roll_back_savepoint(
    connection: &mut PgConnection,
    name: &'static str,
) -> Result<(), StoreError> {
    savepoint_statement(connection, "ROLLBACK TO SAVEPOINT", name).await?;
    savepoint_statement(connection, "RELEASE SAVEPOINT", name).await
}

async fn savepoint_statement(
    connection: &mut PgConnection,
    verb: &'static str,
    name: &'static str,
) -> Result<(), StoreError> {
    sqlx::query(sqlx::AssertSqlSafe(format!("{verb} {name}")))
        .execute(connection)
        .await
        .map_err(StoreError::query)?;
    Ok(())
}

/// What applying one item did.
pub(crate) enum Tried<R> {
    /// Its writes stay.
    Kept,
    /// It is refused, and its writes must go: an expected refusal found only once it was applied.
    Refused(R),
}

/// The items of a unit, applied one by one, each kept or refused alone ([`apply_isolated`]).
pub(crate) trait Isolated {
    /// Why an item is refused.
    type Refusal: Clone;
    /// Where the caller's own state (outcomes, counts) stands, to go back to.
    type Mark;

    fn mark(&self) -> Self::Mark;

    /// Goes back to `mark`: the run since it rolled back.
    fn restore(&mut self, mark: Self::Mark);

    /// Applies item `index`. An error refuses the item unless it is retryable, which fails the
    /// unit.
    async fn apply(
        &mut self,
        connection: &mut PgConnection,
        index: usize,
    ) -> Result<Tried<Self::Refusal>, StoreError>;

    /// The refusal of an item whose apply failed with `error`.
    fn failed(&self, error: StoreError) -> Self::Refusal;

    /// Records that item `index` is refused.
    async fn reject(
        &mut self,
        connection: &mut PgConnection,
        index: usize,
        refusal: &Self::Refusal,
    ) -> Result<(), StoreError>;
}

/// Applies items `0..count` in order, each kept or refused alone, in the caller's transaction,
/// releasing at most `runs` savepoints (at least one).
///
/// They are applied in one savepoint, a run, as long as each is kept. When one is refused, the
/// run rolls back and is applied again with that item rejected instead of applied; the run is
/// released after the item, and the next one starts there, once it is long enough (below). So an
/// item is refused exactly when applying it after the items kept before it fails, as with a
/// savepoint per item, and the writes keep their order: the items kept before a refusal, its
/// reject, then the items after it.
///
/// A run is long enough to end at a refusal when it covers its share of the items left, split
/// over the runs left; the last run allowed never ends early. A refusal in a run too short goes
/// on in the same run, which is applied again from its start. So the refusals of a unit spread
/// over the runs it may release, and an item is applied again about once per refusal in its run.
///
/// A run is a savepoint of its own name, not a nested `sqlx` transaction, so a rolled-back run
/// gives its id back ([`roll_back_savepoint`]). An error it returns (a retryable one, or one
/// recording a refusal) leaves the run open: the caller's transaction must roll back.
pub(crate) async fn apply_isolated<I: Isolated>(
    connection: &mut PgConnection,
    items: &mut I,
    count: usize,
    runs: usize,
) -> Result<(), StoreError> {
    const RUN: &str = "isolated_run";
    let mut refused: Vec<Option<I::Refusal>> = vec![None; count];
    let mut left = runs.max(1);
    let mut start = 0;
    // Where the run is released: after a refusal, or at the end.
    let mut end = count;
    while start < count {
        let mark = items.mark();
        begin_savepoint(connection, RUN).await?;
        let mut newly = None;
        for (index, known) in refused.iter().enumerate().take(end).skip(start) {
            if let Some(refusal) = known {
                items.reject(connection, index, refusal).await?;
                continue;
            }
            match items.apply(connection, index).await {
                Ok(Tried::Kept) => {}
                Ok(Tried::Refused(refusal)) => {
                    newly = Some((index, refusal));
                    break;
                }
                Err(error) if error.is_retryable() => return Err(error),
                Err(error) => {
                    newly = Some((index, items.failed(error)));
                    break;
                }
            }
        }
        match newly {
            None => {
                release_savepoint(connection, RUN).await?;
                left = left.saturating_sub(1);
                start = end;
                end = count;
            }
            Some((index, refusal)) => {
                roll_back_savepoint(connection, RUN).await?;
                items.restore(mark);
                refused[index] = Some(refusal);
                if left > 1 && index + 1 - start >= (count - start).div_ceil(left) {
                    end = index + 1;
                }
            }
        }
    }
    Ok(())
}

/// What the tests read of a transaction's subtransactions.
#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    use sqlx::{PgConnection, PgPool};

    /// How many subtransaction ids this connection's backend caches (`pg_stat_get_backend_subxact`,
    /// PostgreSQL 16), and whether it overflowed the 64. Read after `pg_stat_clear_snapshot()`:
    /// the backends' status is otherwise read once per transaction.
    const OWN_SUBXACTS: &str = "SELECT s.subxact_count, s.subxact_overflowed \
         FROM pg_stat_get_backend_idset() AS id, pg_stat_get_backend_pid(id) AS pid, \
              pg_stat_get_backend_subxact(id) AS s \
         WHERE pid = pg_backend_pid()";

    /// [`OWN_SUBXACTS`] of `connection`, read inside its transaction.
    pub async fn subxacts(connection: &mut PgConnection) -> (i32, bool) {
        sqlx::query("SELECT pg_stat_clear_snapshot()")
            .execute(&mut *connection)
            .await
            .unwrap();
        sqlx::query_as(OWN_SUBXACTS)
            .fetch_one(connection)
            .await
            .unwrap()
    }

    /// From now on, every transaction that writes `table` records, as it commits and from inside
    /// it, [`OWN_SUBXACTS`]: a deferred constraint trigger fires just before the commit, after
    /// the transaction's last savepoint. `table` is a fixed name.
    pub async fn record_at_commit(pool: &PgPool, table: &str) {
        for statement in [
            "CREATE TABLE IF NOT EXISTS subxact_probe \
                 (subxacts integer NOT NULL, overflowed boolean NOT NULL)"
                .to_owned(),
            format!(
                "CREATE OR REPLACE FUNCTION subxact_probe() RETURNS trigger LANGUAGE plpgsql \
                 AS $$ BEGIN PERFORM pg_stat_clear_snapshot(); \
                 INSERT INTO subxact_probe {OWN_SUBXACTS}; RETURN NULL; END $$"
            ),
            format!(
                "CREATE CONSTRAINT TRIGGER subxact_probe AFTER INSERT OR UPDATE OR DELETE \
                 ON {table} DEFERRABLE INITIALLY DEFERRED FOR EACH ROW \
                 EXECUTE FUNCTION subxact_probe()"
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .execute(pool)
                .await
                .unwrap();
        }
    }

    /// From now on, a row written to `table` that matches `condition` (on `NEW`) fails its
    /// statement with an error no retry cures: a record the store cannot apply. Both are fixed
    /// text.
    pub async fn poison(pool: &PgPool, table: &str, condition: &str) {
        for statement in [
            "CREATE OR REPLACE FUNCTION subxact_poison() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'poisoned'; END $$"
                .to_owned(),
            format!(
                "CREATE TRIGGER subxact_poison BEFORE INSERT OR UPDATE ON {table} \
                 FOR EACH ROW WHEN ({condition}) EXECUTE FUNCTION subxact_poison()"
            ),
        ] {
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .execute(pool)
                .await
                .unwrap();
        }
    }

    /// The most subtransaction ids a recorded transaction cached, whether any overflowed, and how
    /// many rows were recorded.
    pub async fn recorded(pool: &PgPool) -> (i32, bool, i64) {
        sqlx::query_as(
            "SELECT COALESCE(max(subxacts), 0), COALESCE(bool_or(overflowed), false), count(*) \
             FROM subxact_probe",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use sqlx::{PgConnection, PgPool};

    use super::testing::subxacts;
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Fate {
        Kept,
        /// Refused by what applying it found.
        Refused,
        /// Refused because a statement failed.
        Failed,
    }

    fn fate(index: usize) -> Fate {
        if index % 7 == 3 {
            Fate::Refused
        } else if index % 11 == 5 {
            Fate::Failed
        } else {
            Fate::Kept
        }
    }

    /// Items that each write a row, refused (by [`fate`]) after writing it.
    #[derive(Default)]
    struct Rows {
        kept: Vec<usize>,
        rejected: Vec<usize>,
        applied: usize,
    }

    impl Isolated for Rows {
        type Refusal = Fate;
        type Mark = (usize, usize);

        fn mark(&self) -> (usize, usize) {
            (self.kept.len(), self.rejected.len())
        }

        fn restore(&mut self, (kept, rejected): (usize, usize)) {
            self.kept.truncate(kept);
            self.rejected.truncate(rejected);
        }

        async fn apply(
            &mut self,
            connection: &mut PgConnection,
            index: usize,
        ) -> Result<Tried<Fate>, StoreError> {
            self.applied += 1;
            sqlx::query("INSERT INTO isolated (item, rejected) VALUES ($1, false)")
                .bind(i32::try_from(index).unwrap())
                .execute(&mut *connection)
                .await
                .map_err(StoreError::query)?;
            match fate(index) {
                Fate::Kept => {
                    self.kept.push(index);
                    Ok(Tried::Kept)
                }
                Fate::Refused => Ok(Tried::Refused(Fate::Refused)),
                Fate::Failed => {
                    sqlx::query("SELECT 1 / 0")
                        .execute(connection)
                        .await
                        .map_err(StoreError::query)?;
                    Ok(Tried::Kept)
                }
            }
        }

        fn failed(&self, error: StoreError) -> Fate {
            assert!(!error.is_retryable());
            Fate::Failed
        }

        async fn reject(
            &mut self,
            connection: &mut PgConnection,
            index: usize,
            refusal: &Fate,
        ) -> Result<(), StoreError> {
            assert_eq!(*refusal, fate(index));
            sqlx::query("INSERT INTO isolated (item, rejected) VALUES ($1, true)")
                .bind(i32::try_from(index).unwrap())
                .execute(connection)
                .await
                .map_err(StoreError::query)?;
            self.rejected.push(index);
            Ok(())
        }
    }

    /// Applies `count` items with `runs` allowed, and checks they end as with a savepoint each:
    /// the same rows in the same order and the same state, and no more ids than runs.
    async fn check(pool: &PgPool, count: usize, runs: usize) -> usize {
        sqlx::query("TRUNCATE isolated")
            .execute(pool)
            .await
            .unwrap();
        let mut tx = crate::begin(pool).await.unwrap();
        let mut rows = Rows::default();
        apply_isolated(&mut tx, &mut rows, count, runs)
            .await
            .unwrap();
        let (cached, overflowed) = subxacts(&mut tx).await;
        crate::commit(tx).await.unwrap();

        let written: Vec<(i32, bool)> =
            sqlx::query_as("SELECT item, rejected FROM isolated ORDER BY seq")
                .fetch_all(pool)
                .await
                .unwrap();
        let expected: Vec<(i32, bool)> = (0..count)
            .map(|index| (i32::try_from(index).unwrap(), fate(index) != Fate::Kept))
            .collect();
        assert_eq!(written, expected, "{count} items, {runs} runs");
        let (kept, rejected): (Vec<usize>, Vec<usize>) =
            (0..count).partition(|index| fate(*index) == Fate::Kept);
        assert_eq!((rows.kept, rows.rejected), (kept, rejected));
        assert!(
            !overflowed && usize::try_from(cached).unwrap() <= runs.max(1),
            "{count} items, {runs} runs: {cached} ids"
        );
        rows.applied
    }

    #[sqlx::test]
    #[ignore = "requires PostgreSQL; set DATABASE_URL"]
    async fn isolated_items_end_as_with_a_savepoint_each_within_their_runs(pool: PgPool) {
        sqlx::query(
            "CREATE TABLE isolated (seq bigserial PRIMARY KEY, item integer NOT NULL, \
                 rejected boolean NOT NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (count, runs) in [
            (0, MAX_RUNS),
            (1, MAX_RUNS),
            (30, MAX_RUNS),
            (200, 10),
            (200, 1),
        ] {
            check(&pool, count, runs).await;
        }
        // A fifth of a thousand items refused, spread over the runs: no item is applied again
        // once per refusal of the unit.
        let applied = check(&pool, 1000, MAX_RUNS).await;
        assert!(applied <= 4 * 1000, "{applied} applies of 1000 items");
    }
}
