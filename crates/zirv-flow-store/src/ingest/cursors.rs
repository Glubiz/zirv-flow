//! Per-bucket id cursors for the inbox take (architecture 7.1). Step one of a take reads each
//! bucket it owns through `(workspace_id, affinity, id)`, oldest row first. That index also keeps
//! the entries of rows that earlier batches deleted, until vacuum removes them, and vacuum cannot
//! remove them while an older snapshot is still open. A long refresh kept one open for minutes,
//! and every take walked thousands of dead entries to reach its first live row (Investigation Q,
//! mechanism 1). With a cursor, a take reads a bucket from `id > after`, past the rows already
//! taken.
//!
//! Ids come from a sequence but commit out of order, so a row whose id is below one a take has
//! already seen can still become visible later: a straggler. Every row must still be taken, so:
//!
//! - **The cursor trails.** It never passes an id drawn less than [`MARGIN`] before the take that
//!   moves it began. A take records the newest id it saw and when its pick returned, and every id
//!   up to that one was drawn by then. A cursor moves to at most the highest such id seen at least
//!   the margin before. A straggler whose append commits within the margin of drawing its id is
//!   therefore above the cursor, and the next take finds it as it does today.
//! - **A full walk.** Every [`PERIOD`], a bucket's next take reads it from its first row again and
//!   resets the cursor from what that walk proves. A take that finds nothing above its cursors does
//!   the same at once, in the same transaction. That take ran because the requeue probe, which
//!   reads without a cursor, saw a row, so the shard's next run finds a lone straggler at once.
//!   Otherwise a straggler outside the margin waits at most one period, plus the batch in progress,
//!   behind the rows its bucket's takes keep finding.
//! - **A blocked bucket.** A take stops a bucket at its first row it could not lock (an erasure
//!   holds it, or another drain took it meanwhile). Its cursor then stays below that row, so the
//!   row is picked again, as today.
//!
//! Only a committed batch moves a cursor: a rollback leaves every row in place and the cursors
//! where they were. A take never moves a cursor that another take set after it began: a full walk
//! that stopped at a held row may have set it, and the older take's advance would pass that row.
//! The state lives in this process only: a worker restart or a crash starts
//! with no cursors, so its first take of each bucket is a full walk. Cursors are kept per bucket,
//! not per shard, so a shard-count change keeps them valid. Each process keeps its own, so a
//! bucket another process drained meanwhile costs this one only the entries it skips, never a
//! row. Rows the backfill buckets late carry old ids, so a backfill that buckets any row drops
//! the workspace's cursors ([`TakeCursors::forget`]).

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use hashbrown::HashMap;
use uuid::Uuid;
use zirv_flow_core::WorkspaceId;

/// How far behind the ids it has seen a cursor trails: an append that commits within this long of
/// drawing its id is never passed over. The API appends in one statement, with p99 under 0.25 s
/// at the 5x load (Investigation Q).
pub(crate) const MARGIN: Duration = Duration::from_secs(1);

/// How often a bucket's take reads it from its first row again, which bounds how long a straggler
/// outside the margin waits.
pub(crate) const PERIOD: Duration = Duration::from_secs(10);

/// The cursors of every workspace this process drains.
#[derive(Debug)]
pub(crate) struct TakeCursors {
    margin: Duration,
    period: Duration,
    workspaces: Mutex<HashMap<Uuid, Workspace>>,
}

impl Default for TakeCursors {
    fn default() -> Self {
        Self::new(MARGIN, PERIOD)
    }
}

#[derive(Debug, Default)]
struct Workspace {
    /// The newest id a take saw, with when its pick returned, until a later take begins at least
    /// the margin after that; then it is folded into `settled`.
    seen: VecDeque<(Instant, i64)>,
    /// The highest id seen at least the margin before the latest take began: every id up to it
    /// had been drawn by then.
    settled: i64,
    /// Where the latest take's margin ended: samples that returned before it are settled.
    horizon: Option<Instant>,
    buckets: HashMap<i16, Bucket>,
    /// Counts every cursor set and every [`TakeCursors::forget`], so a take can tell what was set
    /// after it began.
    sets: u64,
    /// The count of the latest forget.
    forgot_at: u64,
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    /// Every row of the bucket up to this id is gone, except stragglers the next full walk finds.
    after: i64,
    /// When the bucket's next take reads it from its first row again.
    full_walk_at: Instant,
    /// The workspace's count when `after` was set.
    set_at: u64,
}

/// Where a take starts, from [`TakeCursors::start`].
#[derive(Clone, Debug)]
pub(crate) struct TakeStart {
    /// The id each bucket is read after, in the order of the buckets passed in: 0 reads it from
    /// its first row.
    pub(crate) after: Vec<i64>,
    /// The highest id a cursor may move to after this take: drawn at least the margin before it.
    pub(crate) cap: i64,
    /// The workspace's count of cursor sets when the take began.
    pub(crate) began: u64,
}

/// What a committed take proved about each bucket it read, for [`TakeCursors::advance`].
#[derive(Clone, Debug)]
pub(crate) struct Taken {
    pub(crate) cap: i64,
    /// [`TakeStart::began`].
    pub(crate) began: u64,
    /// When the take's pick returned.
    pub(crate) returned: Instant,
    /// The newest id the pick saw, if any.
    pub(crate) newest: Option<i64>,
    /// Per bucket: the id it was read after (0 for a full walk) and the id up to which every row
    /// visible to the pick was taken; `None` when the pick saw the bucket's whole queue and took
    /// all of it.
    pub(crate) buckets: Vec<(i16, i64, Option<i64>)>,
}

impl TakeCursors {
    pub(crate) fn new(margin: Duration, period: Duration) -> Self {
        Self {
            margin,
            period,
            workspaces: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Uuid, Workspace>> {
        // The state is only ever left consistent between statements, so a panic elsewhere while it
        // was held leaves nothing half-written.
        self.workspaces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Where a take of `buckets` in `workspace` starts now.
    pub(crate) fn start(&self, workspace: WorkspaceId, buckets: &[i16]) -> TakeStart {
        let mut workspaces = self.lock();
        let now = Instant::now();
        let state = workspaces.entry(workspace.as_uuid()).or_default();
        if let Some(horizon) = now.checked_sub(self.margin) {
            state.horizon = Some(state.horizon.map_or(horizon, |old| old.max(horizon)));
            let settled = &mut state.settled;
            state.seen.retain(|(returned, id)| {
                let old = *returned <= horizon;
                if old {
                    *settled = (*settled).max(*id);
                }
                !old
            });
        }
        let after = buckets
            .iter()
            .map(|bucket| {
                state
                    .buckets
                    .get(bucket)
                    .filter(|cursor| now < cursor.full_walk_at)
                    .map_or(0, |cursor| cursor.after)
            })
            .collect();
        TakeStart {
            after,
            cap: state.settled,
            began: state.sets,
        }
    }

    /// Moves `workspace`'s cursors on after a take's batch committed. A bucket whose cursor another
    /// take set after this one began, or whose cursors a forget dropped since, is left as it is:
    /// that take may have stopped below a row this one passed, a row another transaction held, and
    /// moving the cursor back above it would leave the row to the next full walk.
    pub(crate) fn advance(&self, workspace: WorkspaceId, taken: &Taken) {
        let mut workspaces = self.lock();
        let state = workspaces.entry(workspace.as_uuid()).or_default();
        if let Some(newest) = taken.newest {
            if state
                .horizon
                .is_some_and(|horizon| taken.returned <= horizon)
            {
                state.settled = state.settled.max(newest);
            } else {
                state.seen.push_back((taken.returned, newest));
            }
        }
        for &(bucket, after, through) in &taken.buckets {
            let set_since = state.forgot_at > taken.began
                || state
                    .buckets
                    .get(&bucket)
                    .is_some_and(|cursor| cursor.set_at > taken.began);
            if set_since {
                continue;
            }
            let proven = through.map_or(taken.cap, |through| through.min(taken.cap));
            if after == 0 {
                state.sets += 1;
                state.buckets.insert(
                    bucket,
                    Bucket {
                        after: proven.max(0),
                        full_walk_at: taken.returned + self.period,
                        set_at: state.sets,
                    },
                );
            } else if let Some(cursor) = state.buckets.get_mut(&bucket)
                && proven > cursor.after
            {
                state.sets += 1;
                cursor.after = proven;
                cursor.set_at = state.sets;
            }
        }
    }

    /// Drops `workspace`'s cursors, so each bucket's next take reads it from its first row. A take
    /// already under way sets none from what it proved, as its pick missed the rows the backfill
    /// bucketed meanwhile.
    pub(crate) fn forget(&self, workspace: WorkspaceId) {
        if let Some(state) = self.lock().get_mut(&workspace.as_uuid()) {
            state.buckets.clear();
            state.sets += 1;
            state.forgot_at = state.sets;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> WorkspaceId {
        WorkspaceId::from_uuid(Uuid::from_u128(7))
    }

    fn taken(start: &TakeStart, newest: i64, buckets: Vec<(i16, i64, Option<i64>)>) -> Taken {
        Taken {
            cap: start.cap,
            began: start.began,
            returned: Instant::now(),
            newest: Some(newest),
            buckets,
        }
    }

    #[test]
    fn a_first_take_reads_every_bucket_from_its_first_row() {
        let cursors = TakeCursors::new(Duration::ZERO, Duration::from_secs(60));

        let start = cursors.start(ws(), &[3, 7]);

        assert_eq!(start.after, [0, 0]);
        assert_eq!(start.cap, 0);
    }

    #[test]
    fn a_cursor_moves_to_what_the_take_proved_but_never_past_the_margin() {
        let cursors = TakeCursors::new(Duration::ZERO, Duration::from_secs(60));
        let first = cursors.start(ws(), &[3, 7]);
        // The pick saw ids up to 40 and took all of bucket 3 and bucket 7 up to 25.
        cursors.advance(
            ws(),
            &taken(&first, 40, vec![(3, 0, None), (7, 0, Some(25))]),
        );

        let second = cursors.start(ws(), &[3, 7]);
        // Nothing was settled before the first take began, so it could prove nothing.
        assert_eq!(second.after, [0, 0]);
        cursors.advance(
            ws(),
            &taken(&second, 50, vec![(3, 0, None), (7, 0, Some(25))]),
        );

        // Id 40 returned before the second take began, so the second take could move to it.
        let third = cursors.start(ws(), &[3, 7]);
        assert_eq!(third.after, [40, 25]);
    }

    #[test]
    fn ids_seen_within_the_margin_do_not_move_a_cursor() {
        let cursors = TakeCursors::new(Duration::from_secs(3600), Duration::from_secs(60));
        let first = cursors.start(ws(), &[3]);
        cursors.advance(ws(), &taken(&first, 40, vec![(3, 0, None)]));
        let second = cursors.start(ws(), &[3]);
        cursors.advance(ws(), &taken(&second, 90, vec![(3, 0, None)]));

        assert_eq!(cursors.start(ws(), &[3]).after, [0]);
    }

    #[test]
    fn a_cursor_take_only_moves_a_cursor_forward_and_a_full_walk_resets_it() {
        let cursors = TakeCursors::new(Duration::ZERO, Duration::from_secs(60));
        let first = cursors.start(ws(), &[3]);
        cursors.advance(ws(), &taken(&first, 100, vec![(3, 0, None)]));
        let second = cursors.start(ws(), &[3]);
        cursors.advance(ws(), &taken(&second, 100, vec![(3, 0, None)]));
        assert_eq!(cursors.start(ws(), &[3]).after, [100]);

        // A take from 100 that was stopped by a locked row at 60 cannot move the cursor back...
        let behind = cursors.start(ws(), &[3]);
        cursors.advance(ws(), &taken(&behind, 100, vec![(3, 100, Some(59))]));
        assert_eq!(cursors.start(ws(), &[3]).after, [100]);
        // ...but a full walk that stopped there does.
        let walk = cursors.start(ws(), &[3]);
        cursors.advance(ws(), &taken(&walk, 100, vec![(3, 0, Some(59))]));
        assert_eq!(cursors.start(ws(), &[3]).after, [59]);
    }

    #[test]
    fn a_bucket_is_read_from_its_first_row_again_once_the_period_has_passed() {
        let cursors = TakeCursors::new(Duration::ZERO, Duration::ZERO);
        let first = cursors.start(ws(), &[3]);
        cursors.advance(ws(), &taken(&first, 100, vec![(3, 0, None)]));
        let second = cursors.start(ws(), &[3]);
        cursors.advance(ws(), &taken(&second, 100, vec![(3, 0, None)]));

        assert_eq!(cursors.start(ws(), &[3]).after, [0]);
    }

    #[test]
    fn forgetting_a_workspace_starts_its_buckets_from_their_first_rows() {
        let cursors = TakeCursors::new(Duration::ZERO, Duration::from_secs(60));
        let other = WorkspaceId::from_uuid(Uuid::from_u128(8));
        for workspace in [ws(), other] {
            for _ in 0..2 {
                let start = cursors.start(workspace, &[3]);
                cursors.advance(workspace, &taken(&start, 100, vec![(3, 0, None)]));
            }
        }

        cursors.forget(ws());

        assert_eq!(cursors.start(ws(), &[3]).after, [0]);
        assert_eq!(cursors.start(other, &[3]).after, [100]);
    }

    /// Lets either take of a test move a cursor as far as 500: a take of another bucket saw it.
    fn seen_up_to_500(cursors: &TakeCursors) {
        let seed = cursors.start(ws(), &[9]);
        cursors.advance(ws(), &taken(&seed, 500, vec![(9, 0, None)]));
    }

    /// A long take reads bucket 3 through 200. Meanwhile a full walk of the bucket stops at a row
    /// another transaction holds and sets the cursor below it. The long take commits last and must
    /// leave the cursor there, or the held row waits for the next full walk. Both the long take's
    /// kinds are covered: a full walk of its own, and a take from the cursor.
    #[test]
    fn a_take_that_commits_after_a_newer_one_set_the_cursor_leaves_it_there() {
        let cursors = TakeCursors::new(Duration::ZERO, Duration::from_secs(3600));
        seen_up_to_500(&cursors);

        let long = cursors.start(ws(), &[3]);
        let walk = cursors.start(ws(), &[3]);
        assert_eq!(
            (long.after[0], walk.after[0]),
            (0, 0),
            "both walk from the first row"
        );
        cursors.advance(ws(), &taken(&walk, 500, vec![(3, 0, Some(59))]));
        cursors.advance(ws(), &taken(&long, 500, vec![(3, 0, Some(200))]));
        assert_eq!(cursors.start(ws(), &[3]).after, [59], "a long full walk");

        let long = cursors.start(ws(), &[3]);
        // The walk reads from the cursor too, finds nothing above it and walks from the first row.
        let walk = cursors.start(ws(), &[3]);
        assert_eq!((long.after[0], walk.after[0]), (59, 59));
        cursors.advance(ws(), &taken(&walk, 500, vec![(3, 0, Some(40))]));
        cursors.advance(ws(), &taken(&long, 500, vec![(3, 59, Some(200))]));
        assert_eq!(
            cursors.start(ws(), &[3]).after,
            [40],
            "a long take from the cursor"
        );
    }

    /// A take that began before the workspace's cursors were dropped sets none from what it proved:
    /// the rows the backfill bucketed meanwhile have old ids and were not in its pick.
    #[test]
    fn a_take_that_began_before_a_forget_sets_no_cursor() {
        let cursors = TakeCursors::new(Duration::ZERO, Duration::from_secs(3600));
        seen_up_to_500(&cursors);
        let walk = cursors.start(ws(), &[3]);

        cursors.forget(ws());
        cursors.advance(ws(), &taken(&walk, 500, vec![(3, 0, Some(200))]));

        assert_eq!(cursors.start(ws(), &[3]).after, [0]);
    }
}
