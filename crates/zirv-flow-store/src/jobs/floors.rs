//! Where a worker lane's claims start in each priority band (architecture section 14). A claim
//! reads `jobs_ready_idx` from the oldest `run_at` of each band. That index also keeps the entries
//! of jobs claimed since the oldest open snapshot, and a claim used to walk all of them before it
//! reached a due job: 2,545 to 9,556 buffer hits a claim on the sim (Investigation Q,
//! mechanism 1). A floor per band lets a claim start past them: `run_at >= floor`. The drain and
//! the re-evaluation, which a claim takes from `jobs_ready_kind_idx` one at a time, get a floor
//! each.
//!
//! A floor may pass only jobs that are gone, so a claim moves it to at most the `run_at` of the
//! youngest job it took from that band, and never past [`MARGIN`] before the claim's own `now()`.
//! A job is due at its `run_at`, `now()` of the transaction that enqueued it unless set. One
//! that commits after a claim passed its `run_at` is therefore above the floor if its transaction
//! lasted less than the margin. Other ways a job can be due below a floor are:
//!
//! - a longer transaction, or a `run_at` set in the past;
//! - a job given back unstarted, which keeps its `run_at` (a give-back by this process expires
//!   the floors at once, [`ClaimFloors::expire`]);
//! - a row a claim skipped because another transaction held it, and that this transaction left
//!   ready;
//! - a band this lane has never taken a job from, which a floor claim does not read.
//!
//! All of them are claimed by the lane's next full walk: the claim of today, every band from its
//! first entry and the bands found afresh. It runs at the first claim after [`PERIOD`] has passed
//! since the last one. Such a job waits at most the period, plus the time until the lane's next
//! claim. A slot claims as soon as its batch is done, an idle one within the poll interval (1 s),
//! so the wait is about 2 s at most.
//!
//! A floor claim keeps the full walk's order: band by band upwards, by age within a band, at most
//! one drain and one re-evaluation. The capped claim does not use floors. Floors are kept per lane
//! in the worker's memory, so a restart starts with a full walk.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use time::OffsetDateTime;

/// The kinds a claim takes from `jobs_ready_kind_idx` one at a time rather than with their band.
pub(crate) use crate::{DRAIN_JOB_KIND as DRAIN_KIND, REEVALUATE_JOB_KIND as REEVALUATE_KIND};

/// How far behind its claim's `now()` a floor stays: a job enqueued by a transaction shorter than
/// this is never passed over.
pub const MARGIN: Duration = Duration::from_secs(1);

/// How often a lane's claim reads every band from its first entry again.
pub const PERIOD: Duration = Duration::from_secs(1);

/// The floors of one worker lane, shared by its slots.
#[derive(Debug)]
pub struct ClaimFloors {
    margin: Duration,
    period: Duration,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    /// The bands this lane has taken a job from, each with its floor; `None` reads the band from
    /// its first entry. The default band starts known, so the first job of an idle lane does not
    /// wait for a full walk.
    bands: BTreeMap<i16, Option<OffsetDateTime>>,
    drain: Option<OffsetDateTime>,
    reevaluate: Option<OffsetDateTime>,
    /// When the next claim walks every band from its start; `None` is now.
    full_walk_at: Option<Instant>,
}

/// Where a claim starts, from [`ClaimFloors::plan`].
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ClaimFrom {
    /// The full walk: every band found afresh and read from its first entry.
    Start,
    /// The floor claim: the known bands upwards, each from its floor.
    Floors {
        bands: Vec<i16>,
        floors: Vec<Option<OffsetDateTime>>,
        drain: Option<OffsetDateTime>,
        reevaluate: Option<OffsetDateTime>,
    },
}

/// One job a claim took: its band, kind and `run_at`, and the claim's `now()`.
pub(crate) struct Took<'a> {
    pub(crate) priority: i16,
    pub(crate) kind: &'a str,
    pub(crate) run_at: OffsetDateTime,
    pub(crate) now: OffsetDateTime,
}

impl Default for ClaimFloors {
    fn default() -> Self {
        Self::new(MARGIN, PERIOD)
    }
}

impl ClaimFloors {
    /// Floors that trail their claims by `margin`, with a full walk at most every `period`.
    pub fn new(margin: Duration, period: Duration) -> Self {
        Self {
            margin,
            period,
            state: Mutex::new(State {
                bands: BTreeMap::from([(0, None)]),
                drain: None,
                reevaluate: None,
                full_walk_at: None,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Every update is whole before the lock is let go, so a poisoned state is consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes the next claim a full walk: jobs given back unstarted keep their `run_at`, which may
    /// sit below a floor.
    pub fn expire(&self) {
        self.lock().full_walk_at = None;
    }

    /// Where the next claim starts. `bands` is false for a lane whose kinds a band claim never
    /// takes (the drain and the re-evaluation alone), which then reads no band at all.
    pub(crate) fn plan(&self, bands: bool) -> ClaimFrom {
        let mut state = self.lock();
        let now = Instant::now();
        if state.full_walk_at.is_none_or(|at| now >= at) {
            // Set here, not when the walk returns, so the lane's other slots claim from the floors
            // meanwhile instead of walking too.
            state.full_walk_at = Some(now + self.period);
            return ClaimFrom::Start;
        }
        let (bands, floors) = if bands {
            state
                .bands
                .iter()
                .map(|(band, floor)| (*band, *floor))
                .unzip()
        } else {
            (Vec::new(), Vec::new())
        };
        ClaimFrom::Floors {
            bands,
            floors,
            drain: state.drain,
            reevaluate: state.reevaluate,
        }
    }

    /// Moves the floors on after a claim took `claimed`. After a full walk a band's floor is set
    /// from what the walk took, lower than before when the walk found jobs below it; after a floor
    /// claim it only moves up.
    pub(crate) fn record(&self, full: bool, claimed: &[Took<'_>]) {
        if claimed.is_empty() {
            return;
        }
        // The youngest job taken from each place, and the claim's now().
        let mut bands: BTreeMap<i16, (OffsetDateTime, OffsetDateTime)> = BTreeMap::new();
        let (mut drain, mut reevaluate) = (None, None);
        for took in claimed {
            let youngest = match took.kind {
                DRAIN_KIND => &mut drain,
                REEVALUATE_KIND => &mut reevaluate,
                _ => {
                    let entry = bands
                        .entry(took.priority)
                        .or_insert((took.run_at, took.now));
                    entry.0 = entry.0.max(took.run_at);
                    continue;
                }
            };
            let (run_at, _) = youngest.get_or_insert((took.run_at, took.now));
            *run_at = (*run_at).max(took.run_at);
        }
        let floor = |(run_at, now): (OffsetDateTime, OffsetDateTime)| run_at.min(now - self.margin);
        let moved = |old: Option<OffsetDateTime>, new: OffsetDateTime| {
            Some(if full {
                new
            } else {
                old.map_or(new, |old| old.max(new))
            })
        };
        let mut state = self.lock();
        for (band, youngest) in bands {
            let old = state.bands.get(&band).copied().flatten();
            state.bands.insert(band, moved(old, floor(youngest)));
        }
        if let Some(youngest) = drain {
            state.drain = moved(state.drain, floor(youngest));
        }
        if let Some(youngest) = reevaluate {
            state.reevaluate = moved(state.reevaluate, floor(youngest));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1_000_000 + seconds)
    }

    fn took(priority: i16, kind: &str, run_at: i64, now: i64) -> Took<'_> {
        Took {
            priority,
            kind,
            run_at: at(run_at),
            now: at(now),
        }
    }

    fn floors_of(plan: ClaimFrom) -> (Vec<(i16, Option<OffsetDateTime>)>, Option<OffsetDateTime>) {
        match plan {
            ClaimFrom::Floors {
                bands,
                floors,
                drain,
                ..
            } => (bands.into_iter().zip(floors).collect(), drain),
            ClaimFrom::Start => panic!("a full walk"),
        }
    }

    #[test]
    fn the_first_claim_is_a_full_walk_and_the_next_reads_the_default_band_from_its_start() {
        let floors = ClaimFloors::new(Duration::from_secs(1), Duration::from_secs(60));

        assert_eq!(floors.plan(true), ClaimFrom::Start);
        assert_eq!(floors_of(floors.plan(true)), (vec![(0, None)], None));
    }

    #[test]
    fn a_floor_trails_the_youngest_job_taken_and_the_claims_now_by_the_margin() {
        let floors = ClaimFloors::new(Duration::from_secs(1), Duration::from_secs(60));
        assert_eq!(floors.plan(true), ClaimFrom::Start);

        // A backlog: the youngest job taken is far behind now, so the floor is that job.
        floors.record(true, &[took(0, "a", 10, 100), took(0, "a", 20, 100)]);
        assert_eq!(floors_of(floors.plan(true)).0, [(0, Some(at(20)))]);
        // Caught up: the youngest job was due just now, so the floor is the margin behind now.
        floors.record(false, &[took(0, "a", 200, 200)]);
        assert_eq!(floors_of(floors.plan(true)).0, [(0, Some(at(199)))]);
        // A floor claim never moves a floor down.
        floors.record(false, &[took(0, "a", 150, 300)]);
        assert_eq!(floors_of(floors.plan(true)).0, [(0, Some(at(199)))]);
    }

    #[test]
    fn a_full_walk_lowers_a_floor_to_a_job_it_found_below_it() {
        let floors = ClaimFloors::new(Duration::from_secs(1), Duration::from_secs(60));
        assert_eq!(floors.plan(true), ClaimFrom::Start);
        floors.record(true, &[took(0, "a", 200, 200)]);

        floors.record(true, &[took(0, "a", 50, 201)]);

        assert_eq!(floors_of(floors.plan(true)).0, [(0, Some(at(50)))]);
    }

    #[test]
    fn with_a_period_of_zero_every_claim_is_a_full_walk() {
        let floors = ClaimFloors::new(Duration::from_secs(1), Duration::ZERO);

        assert_eq!(floors.plan(true), ClaimFrom::Start);
        assert_eq!(floors.plan(true), ClaimFrom::Start);
    }

    #[test]
    fn bands_are_learned_from_the_jobs_taken_and_planned_upwards() {
        let floors = ClaimFloors::new(Duration::from_secs(1), Duration::from_secs(60));
        assert_eq!(floors.plan(true), ClaimFrom::Start);
        floors.record(true, &[took(5, "a", 10, 100), took(-3, "b", 30, 100)]);

        let (bands, _) = floors_of(floors.plan(true));

        assert_eq!(
            bands,
            [(-3, Some(at(30))), (0, None), (5, Some(at(10)))],
            "the default band stays known"
        );
    }

    #[test]
    fn the_drain_and_the_reevaluation_have_floors_of_their_own_and_are_not_bands() {
        let floors = ClaimFloors::new(Duration::from_secs(1), Duration::from_secs(60));
        assert_eq!(floors.plan(false), ClaimFrom::Start);
        floors.record(
            true,
            &[
                took(0, DRAIN_KIND, 40, 100),
                took(-1, REEVALUATE_KIND, 60, 100),
            ],
        );

        assert_eq!(
            floors.plan(false),
            ClaimFrom::Floors {
                bands: Vec::new(),
                floors: Vec::new(),
                drain: Some(at(40)),
                reevaluate: Some(at(60)),
            },
            "a lane of drains reads no band"
        );
        assert_eq!(floors_of(floors.plan(true)).0, [(0, None)]);
    }

    #[test]
    fn expiring_makes_the_next_claim_a_full_walk_and_only_one_slot_walks() {
        let floors = ClaimFloors::new(Duration::from_secs(1), Duration::from_secs(60));
        assert_eq!(floors.plan(true), ClaimFrom::Start);
        assert_ne!(floors.plan(true), ClaimFrom::Start, "one walk per period");

        floors.expire();

        assert_eq!(floors.plan(true), ClaimFrom::Start);
        assert_ne!(floors.plan(true), ClaimFrom::Start);
    }
}
