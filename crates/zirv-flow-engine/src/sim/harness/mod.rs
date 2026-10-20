//! The deterministic flow simulation harness (architecture section 18, layer 1): thousands of
//! enrollments over 90 virtual days, on generated graphs that use every node kind, against an
//! in-memory model of the commit protocol (9.2 and 9.3), with crashes injected inside and after
//! the commit of every step. It is test-only and adds no runtime surface; layer 2 runs the same
//! protocol on PostgreSQL.
//!
//! The same seed always gives the same run: SplitMix64 is the only source of randomness, the
//! clock is virtual, and every table is an ordered map, so no hash order can reach the outcome.
//! A failure prints its seed and population. To reproduce one, set `ZIRV_FLOW_SIM_SEED` and
//! `ZIRV_FLOW_SIM_ENROLLMENTS` and run `cargo test -p zirv-flow-engine --lib sim::harness::`.

mod db;
mod invariants;
mod run;

use super::{generate, rng};

use std::collections::BTreeSet;
use std::env;

/// The environment variable that sets the seed, a `u64`. The nightly run sets it at random and
/// prints it.
pub(super) const SEED_VAR: &str = "ZIRV_FLOW_SIM_SEED";
/// The environment variable that sets the number of enrollments.
pub(super) const ENROLLMENTS_VAR: &str = "ZIRV_FLOW_SIM_ENROLLMENTS";

/// The seeds CI runs, and the population of each.
const FIXED_SEEDS: [u64; 3] = [0x5eed_2030_0101, 0x0f10_e157, 0x90_da75];
const DEFAULT_ENROLLMENTS: usize = 3000;

/// How the process dies during the run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CrashMode {
    Never,
    /// Each attempt dies with probability `1 / one_in`, at a random point.
    Random {
        one_in: u64,
    },
    /// Attempt `n` of every job dies at point `n`: after each effect in turn, then after the
    /// commit, then the job goes through.
    EveryEffect,
}

/// A deliberately broken model, to prove the checks bite.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Defect {
    /// No idempotency at all: a redelivered job reuses what its first claim read, and the commit
    /// has no version compare-and-set, no step execution check and no unique keys.
    NoIdempotency,
    /// External effects are never enqueued.
    DropOutbox,
    /// A wait never enqueues its timer.
    LoseWake,
    /// Timers are enqueued an hour before their `wake_at`.
    EarlyTimer,
    /// Internal effects are applied outside the transaction, so a rollback cannot take them back.
    LeakyRollback,
}

#[derive(Clone, Debug)]
pub(super) struct Config {
    pub(super) seed: u64,
    pub(super) enrollments: usize,
    pub(super) crash: CrashMode,
    pub(super) defect: Option<Defect>,
    /// Jobs of one enrollment that are due together are claimed together and commit in a seeded
    /// order: the event/timeout race.
    pub(super) races: bool,
}

impl Config {
    pub(super) const fn new(seed: u64, enrollments: usize) -> Self {
        Self {
            seed,
            enrollments,
            crash: CrashMode::Random { one_in: 6 },
            defect: None,
            races: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum Invariant {
    /// The engine or the generator failed on a valid graph; not one of the four.
    Engine,
    /// 1: no internal effect is applied twice.
    InternalEffectsOnce,
    /// 2: every external effect is enqueued exactly once, under its stable key.
    ExternalEffectsOnce,
    /// 3: no timer fires before its `wake_at`.
    TimerNotEarly,
    /// 4: every enrollment has ended or waits on a pending wake or a registered event wait.
    Liveness,
}

#[derive(Clone, Debug)]
pub(super) struct Violation {
    pub(super) invariant: Invariant,
    pub(super) detail: String,
}

/// What a run did, for the assertions that the run was not vacuous.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct Stats {
    pub(super) enrollments: usize,
    pub(super) commits: usize,
    pub(super) stale: usize,
    pub(super) races: usize,
    pub(super) crashes_in_tx: usize,
    pub(super) crashes_after_commit: usize,
    /// Steps that committed without first being killed at each of their `effects + 2`
    /// in-transaction points; only counted when every effect is a crash point.
    pub(super) partial_sweeps: usize,
    pub(super) queue_left: usize,
    /// The node `type`s that committed in a step.
    pub(super) kinds: BTreeSet<&'static str>,
}

#[derive(Debug, Default)]
pub(super) struct Report {
    /// The first violations, in order found.
    pub(super) violations: Vec<Violation>,
    pub(super) violation_count: usize,
    /// Every invariant that failed at least once, kept or not.
    pub(super) failed: BTreeSet<Invariant>,
    pub(super) stats: Stats,
    /// A digest of the final tables: equal for equal runs.
    pub(super) fingerprint: u64,
}

/// The most violations kept in a report; the count is exact.
const KEPT: usize = 20;

impl Report {
    /// The kept violations, one per line.
    fn lines(&self) -> String {
        self.violations
            .iter()
            .map(|violation| {
                format!(
                    "  invariant {:?}: {}",
                    violation.invariant, violation.detail
                )
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            )
    }

    pub(super) fn push(&mut self, violation: Violation) {
        self.violation_count += 1;
        self.failed.insert(violation.invariant);
        if self.violations.len() < KEPT {
            self.violations.push(violation);
        }
    }

    pub(super) fn has(&self, invariant: Invariant) -> bool {
        self.failed.contains(&invariant)
    }

    /// Panics with the seed, the population and the first violations if any invariant failed.
    pub(super) fn assert_clean(&self, config: &Config) {
        assert!(
            self.violation_count == 0,
            "simulation failed: seed {}, population {} enrollments, {} violations; rerun with \
             {SEED_VAR}={} {ENROLLMENTS_VAR}={}\n{}",
            config.seed,
            config.enrollments,
            self.violation_count,
            config.seed,
            config.enrollments,
            self.lines(),
        );
    }
}

/// Runs a population.
pub(super) fn simulate(config: &Config) -> Report {
    run::run(config)
}

/// The number in a variable, or `None` when it is unset or blank. Any other value panics: a run
/// that silently fell back would claim to reproduce a seed it never ran.
fn parse_number<T: std::str::FromStr>(var: &str, text: Option<&str>) -> Option<T> {
    let text = text.map(str::trim).filter(|text| !text.is_empty())?;
    let number = text.parse();
    Some(number.unwrap_or_else(|_| panic!("{var} must be a number that fits, not {text:?}")))
}

/// The seeds to run: the one in [`SEED_VAR`], else the fixed ones.
pub(super) fn seeds(text: Option<&str>) -> Vec<u64> {
    parse_number(SEED_VAR, text).map_or_else(|| FIXED_SEEDS.to_vec(), |seed| vec![seed])
}

/// The population: [`ENROLLMENTS_VAR`], else a modest default.
pub(super) fn enrollments(text: Option<&str>) -> usize {
    parse_number(ENROLLMENTS_VAR, text).map_or(DEFAULT_ENROLLMENTS, |count| {
        assert!(count > 0, "{ENROLLMENTS_VAR} must be positive");
        count
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A population small enough for the checks that run many variants.
    const SMALL: usize = 160;

    fn env_seeds() -> Vec<u64> {
        seeds(env::var(SEED_VAR).ok().as_deref())
    }

    fn env_enrollments() -> usize {
        enrollments(env::var(ENROLLMENTS_VAR).ok().as_deref())
    }

    #[test]
    fn the_environment_overrides_parse() {
        assert_eq!(seeds(None), FIXED_SEEDS.to_vec());
        assert_eq!(seeds(Some(" ")), FIXED_SEEDS.to_vec());
        assert_eq!(seeds(Some(" 42 ")), vec![42]);
        assert_eq!(seeds(Some("0")), vec![0]);
        assert_eq!(enrollments(None), DEFAULT_ENROLLMENTS);
        assert_eq!(enrollments(Some("5000")), 5000);
    }

    /// A value the run cannot use stops it rather than running something else under its name.
    #[test]
    fn an_unusable_override_stops_the_run() {
        for (parse, text) in [
            (seeds_len as fn(Option<&str>) -> usize, "not a number"),
            (seeds_len, "99999999999999999999"),
            (enrollments, "0"),
            (enrollments, "-5"),
        ] {
            let outcome = std::panic::catch_unwind(|| parse(Some(text)));
            assert!(outcome.is_err(), "{text:?} was accepted");
        }
    }

    fn seeds_len(text: Option<&str>) -> usize {
        seeds(text).len()
    }

    /// The main check: the fixed seeds in CI, or the seed and size from the environment (the
    /// nightly job and a reproduction). Random crashes everywhere, with event/timeout races.
    #[test]
    fn the_population_holds_all_four_invariants() {
        for seed in env_seeds() {
            let config = Config::new(seed, env_enrollments());
            let report = simulate(&config);
            report.assert_clean(&config);
            let stats = &report.stats;
            println!(
                "seed {seed}: {} enrollments, {} commits, {} stale, {} races, {} crashes in \
                 transactions, {} after commit",
                stats.enrollments,
                stats.commits,
                stats.stale,
                stats.races,
                stats.crashes_in_tx,
                stats.crashes_after_commit
            );
            assert_eq!(stats.queue_left, 0);
            assert!(stats.crashes_in_tx > 0 && stats.crashes_after_commit > 0);
            assert!(stats.races > 0 && stats.stale > 0);
            let mut every_kind: Vec<&str> = zirv_flow_core::flow::NODE_TYPES.to_vec();
            every_kind.sort_unstable();
            assert_eq!(
                stats.kinds.iter().copied().collect::<Vec<_>>(),
                every_kind,
                "seed {seed}: some node kind never ran"
            );
        }
    }

    #[test]
    fn a_run_without_crashes_holds_the_invariants() {
        let mut config = Config::new(0xc1ea_2025, SMALL);
        config.crash = CrashMode::Never;
        let report = simulate(&config);
        report.assert_clean(&config);
        assert_eq!(
            report.stats.crashes_in_tx + report.stats.crashes_after_commit,
            0
        );
    }

    /// Every step is killed after each of its effects in turn (and before the first, and after
    /// the last write), then after its commit, and only then let through.
    #[test]
    fn a_crash_after_every_effect_of_every_step_changes_nothing() {
        let mut config = Config::new(0xe7e2_f001, SMALL);
        config.crash = CrashMode::EveryEffect;
        let report = simulate(&config);
        report.assert_clean(&config);
        let stats = &report.stats;
        assert_eq!(stats.partial_sweeps, 0);
        assert_eq!(stats.crashes_after_commit, stats.commits);
        assert!(stats.crashes_in_tx > stats.commits);
    }

    #[test]
    fn the_same_seed_gives_the_same_run() {
        let config = Config::new(77, SMALL);
        let first = simulate(&config);
        let second = simulate(&config);
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.stats, second.stats);
        let other = simulate(&Config::new(78, SMALL));
        assert_ne!(first.fingerprint, other.fingerprint);
    }

    /// The model without idempotency must fail the first invariant, in both of the ways a step
    /// runs twice: a job redelivered after its commit, and two workers racing on one state.
    #[test]
    fn a_runtime_without_idempotency_fails_an_invariant() {
        let mut config = Config::new(0xbad0_0001, SMALL);
        config.defect = Some(Defect::NoIdempotency);
        let report = simulate(&config);
        assert!(
            report.has(Invariant::InternalEffectsOnce),
            "the broken model passed: {}",
            report.lines()
        );

        // With one failure mode at a time, each alone still bites.
        config.races = false;
        config.crash = CrashMode::Random { one_in: 3 };
        assert!(simulate(&config).has(Invariant::InternalEffectsOnce));
        config.races = true;
        config.crash = CrashMode::Never;
        assert!(simulate(&config).has(Invariant::InternalEffectsOnce));
    }

    #[test]
    fn a_runtime_that_drops_external_effects_fails_the_second_invariant() {
        let mut config = Config::new(0xbad0_0002, SMALL);
        config.defect = Some(Defect::DropOutbox);
        assert!(simulate(&config).has(Invariant::ExternalEffectsOnce));
    }

    #[test]
    fn a_runtime_that_loses_timers_fails_the_fourth_invariant() {
        let mut config = Config::new(0xbad0_0003, SMALL);
        config.defect = Some(Defect::LoseWake);
        assert!(simulate(&config).has(Invariant::Liveness));
    }

    #[test]
    fn a_runtime_that_fires_timers_early_fails_the_third_invariant() {
        let mut config = Config::new(0xbad0_0004, SMALL);
        config.defect = Some(Defect::EarlyTimer);
        assert!(simulate(&config).has(Invariant::TimerNotEarly));
    }

    #[test]
    fn a_runtime_that_cannot_roll_back_fails_the_first_invariant() {
        let mut config = Config::new(0xbad0_0005, SMALL);
        config.defect = Some(Defect::LeakyRollback);
        assert!(simulate(&config).has(Invariant::InternalEffectsOnce));
    }

    #[test]
    fn a_failure_names_its_seed_and_population() {
        let config = Config::new(123, 45);
        let mut report = Report::default();
        report.push(Violation {
            invariant: Invariant::Liveness,
            detail: "stuck".into(),
        });
        let caught = std::panic::catch_unwind(|| report.assert_clean(&config));
        let message = caught
            .err()
            .and_then(|payload| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        assert!(message.contains("seed 123"), "{message}");
        assert!(message.contains("45 enrollments"), "{message}");
        assert!(message.contains(&format!("{SEED_VAR}=123")), "{message}");
    }
}
