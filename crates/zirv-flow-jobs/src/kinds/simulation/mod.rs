//! The PostgreSQL runtime simulation (architecture section 18, layer 2 of the `engine` row).
//!
//! The real runtime runs on PostgreSQL: `flow.enroll`, `flow.step` and `webhook.call` jobs (and the
//! `delivery.send` jobs the send nodes queue, which are only counted), with
//! event waits, over 90 virtual days and a few hundred enrollments on generated graphs that use
//! every node kind. Time is a `ManualClock`. Events arrive at seeded virtual times, many of them
//! at, just before or just after the timeout of the wait they answer. Every job is put through
//! at least one seeded failure before its final successful run (see `inject`), the mock
//! destination fails calls too (see `mock`), and wakes are claimed before they are due. After
//! the run the four invariants are checked from the tables, the log of write attempts and the
//! log of the mock destination (see `invariants`).
//!
//! The graphs and the generator come from the engine harness (layer 1) through its
//! `test-support` feature, so both layers draw from the same seeds. The seed is `ZIRV_FLOW_SIM_SEED`
//! (the variable layer 1 reads), and the population is `ZIRV_FLOW_PG_SIM_ENROLLMENTS`; without
//! them the fixed CI seeds and population run. A failure names its seed and population. To
//! reproduce one:
//!
//! ```text
//! ZIRV_FLOW_SIM_SEED=<seed> ZIRV_FLOW_PG_SIM_ENROLLMENTS=<n> cargo test -p zirv-flow-jobs --lib \
//!     kinds::simulation:: -- --ignored --nocapture
//! ```
//!
//! A seed names the scenario (graphs, people, enrollment times, event times, injection plans),
//! not the exact interleaving: job ids are random, and the concurrent duplicates race for real.

mod driver;
mod inject;
mod invariants;
mod mock;
mod world;

use std::env;
use std::time::Instant;

use sqlx::PgPool;

use self::driver::Driver;
use self::world::World;

/// Shared with layer 1: one seed variable for both.
const SEED_VAR: &str = "ZIRV_FLOW_SIM_SEED";
/// Layer 1 has its own population variable (`ZIRV_FLOW_SIM_ENROLLMENTS`, in the hundreds of
/// thousands); a PostgreSQL run is orders of magnitude slower.
const POPULATION_VAR: &str = "ZIRV_FLOW_PG_SIM_ENROLLMENTS";
/// The seeds CI runs.
const FIXED_SEEDS: [u64; 2] = [0x5eed_0163_0001, 0x0163_f10e];
/// The population of each CI seed.
const DEFAULT_POPULATION: usize = 120;
/// The smallest population for which the coverage assertions mean something: below it a rare
/// mode, variant or destination class may not occur at all. A smaller run keeps the invariants
/// and the executions check, prints that it skips the rest, and does not fail on a rare mode.
pub(super) const COVERAGE_MIN_POPULATION: usize = 100;

/// The number in a variable, or `None` when it is unset or blank. Any other value panics: a run
/// that silently fell back would claim to reproduce a seed it never ran.
fn parse_number<T: std::str::FromStr>(var: &str, text: Option<&str>) -> Option<T> {
    let text = text.map(str::trim).filter(|text| !text.is_empty())?;
    let number = text.parse();
    Some(number.unwrap_or_else(|_| panic!("{var} must be a number that fits, not {text:?}")))
}

fn seeds(text: Option<&str>) -> Vec<u64> {
    parse_number(SEED_VAR, text).map_or_else(|| FIXED_SEEDS.to_vec(), |seed| vec![seed])
}

fn population(text: Option<&str>) -> usize {
    parse_number(POPULATION_VAR, text).map_or(DEFAULT_POPULATION, |count| {
        assert!(count > 0, "{POPULATION_VAR} must be positive");
        count
    })
}

#[test]
fn the_environment_overrides_parse() {
    assert_eq!(seeds(None), FIXED_SEEDS.to_vec());
    assert_eq!(seeds(Some(" ")), FIXED_SEEDS.to_vec());
    assert_eq!(seeds(Some(" 42 ")), vec![42]);
    assert_eq!(population(None), DEFAULT_POPULATION);
    assert_eq!(population(Some("1000")), 1000);
    for text in ["x", "-1", "99999999999999999999"] {
        assert!(
            std::panic::catch_unwind(|| seeds(Some(text))).is_err(),
            "{text:?} was accepted as a seed"
        );
    }
    assert!(std::panic::catch_unwind(|| population(Some("0"))).is_err());
}

/// Runs the CI seed of `ordinal` in the database of the calling test. Each fixed seed is a test of
/// its own, so nextest runs them in parallel and each stays within the default slow-test
/// timeout. With `ZIRV_FLOW_SIM_SEED` set, the first test runs that seed and the others have
/// nothing to do.
async fn simulate(pool: PgPool, ordinal: usize) {
    let seeds = seeds(env::var(SEED_VAR).ok().as_deref());
    let Some(seed) = seeds.get(ordinal).copied() else {
        println!("{SEED_VAR} names one seed: the test of CI seed {ordinal} has nothing to run");
        return;
    };
    let population = population(env::var(POPULATION_VAR).ok().as_deref());
    println!(
        "PostgreSQL flow simulation: seed {seed}, {population} enrollments, 90 virtual days \
         (rerun with {SEED_VAR}={seed} {POPULATION_VAR}={population})"
    );
    let started = Instant::now();
    let world = World::build(pool.clone(), ordinal, seed, population).await;
    let built = started.elapsed();
    let mut driver = Driver::new(&world);
    driver.run().await;
    let ran = started.elapsed();
    let found = invariants::check(&world, &driver, population).await;
    println!(
        "seed {seed}: setup {built:.1?}, run {:.1?}, checks {:.1?}",
        ran - built,
        started.elapsed() - ran
    );
    for (what, count) in &driver.counts {
        println!("  {what}: {count}");
    }
    for (what, count) in &found.summary {
        println!("  {what}: {count}");
    }
    assert!(
        found.violations.is_empty() && found.run_problems.is_empty(),
        "simulation failed: seed {seed}, population {population} enrollments, {} violations; \
         rerun with {SEED_VAR}={seed} {POPULATION_VAR}={population}\n{}",
        found.summary.get("violations").copied().unwrap_or(0),
        found
            .run_problems
            .iter()
            .map(|problem| format!("during the run: {problem}"))
            .chain(found.violations.iter().cloned())
            .collect::<Vec<_>>()
            .join("\n")
    );
    let count = |what: &str| driver.counts.get(what).copied().unwrap_or(0);

    // The issue asks for a failure after every effect: every execution meets a real crash (a
    // rollback or a commit without the ack that happened, or an overlap) before its final run.
    // An answer of the destination that makes the handler return a retry is not one. Jobs whose
    // attempts run out count on both sides. This holds at any population.
    println!(
        "seed {seed}: {} of {} job executions were injected before their final run",
        count("injected executions"),
        count("executions")
    );
    assert!(
        count("executions") > 0 && count("injected executions") == count("executions"),
        "seed {seed}, population {population}: {} of {} job executions met a real crash",
        count("injected executions"),
        count("executions")
    );

    // The coverage assertions need enough jobs, calls and waits for every mode, variant and
    // destination class to occur: that takes about a hundred enrollments (the CI default is
    // 120). Below that the invariants above still hold, but a rare mode may not show up, and
    // saying so beats failing on it.
    if population < COVERAGE_MIN_POPULATION {
        println!(
            "seed {seed}: {population} enrollments is below the {COVERAGE_MIN_POPULATION} the \
             coverage assertions need; they are skipped"
        );
        return;
    }
    let need = |what: &str| {
        assert!(
            count(what) > 0,
            "seed {seed}, population {population}: the run never exercised {what:?} (the \
             coverage assertions are meant for at least {COVERAGE_MIN_POPULATION} enrollments)"
        );
    };
    for mode in [
        "early wake claimed",
        "early wake put back",
        "wake moved an enrollment",
        // The destination fails too: calls are retried, some use up their attempts through
        // lease expiry, and the outcome events a call writes twice settle each wait once.
        "webhook.call attempts retried",
        "retries delayed by the Retry-After of the destination",
        "webhook.call jobs dead",
        "waits on a webhook outcome",
        "outcome events that settled a wait",
        "outcome events that settled nothing",
        "duplicate outcome events that settled nothing",
    ] {
        need(mode);
    }
    // The send nodes of the covering graphs ran and queued their sends, which the driver leaves
    // queued (see invariant 2).
    assert!(
        found
            .summary
            .get("send jobs enqueued")
            .copied()
            .unwrap_or(0)
            > 0,
        "seed {seed}, population {population}: no send node queued a message"
    );
    // Every kind of job goes through every mode, and through both concurrent variants.
    for kind in ["flow.step", "webhook.call", "flow.enroll"] {
        for mode in ["rollback", "commit without ack", "concurrent duplicate"] {
            need(&format!("{mode} {kind}"));
        }
        // Fenced: B's claim runs while A's transaction is open, the claim query skips the job,
        // and A's ack is refused. All three happen for every fenced run.
        for what in ["A open while B claimed", "claim skipped", "A's ack refused"] {
            need(&format!("fenced: {what} {kind}"));
        }
        let fenced = count(&format!("fenced: A open while B claimed {kind}"));
        assert!(
            fenced == count(&format!("fenced: claim skipped {kind}"))
                && fenced == count(&format!("fenced: A's ack refused {kind}")),
            "seed {seed}, population {population}: fenced runs of {kind} differ in what they saw"
        );
        // Unfenced: A's ack is refused every time. A step or an enrollment job makes B wait on
        // a row A wrote (the enrollment row, or the enrollments A inserted), so B is blocked on
        // A every time. A webhook call has nothing for B to wait on (A holds no lock on the job
        // row, and the rows A inserted are ones B does not touch), so B finishes first.
        need(&format!("unfenced: A's ack refused {kind}"));
        let refused = count(&format!("unfenced: A's ack refused {kind}"));
        let (waits, expected_name, other_name) = if kind == "webhook.call" {
            (
                count(&format!("unfenced: B finished without waiting on A {kind}")),
                "finished without waiting",
                "blocked",
            )
        } else {
            (
                count(&format!("unfenced: B blocked on A {kind}")),
                "blocked",
                "finished without waiting",
            )
        };
        assert!(
            waits == refused,
            "seed {seed}, population {population}: of {refused} unfenced runs of {kind}, {waits} \
             were B {expected_name}; the rest were B {other_name} or neither"
        );
    }
    for what in [
        "webhook outcomes: succeeded",
        "webhook outcomes: failed on the last attempt",
    ] {
        assert!(
            found.summary.get(what).copied().unwrap_or(0) > 0,
            "seed {seed}, population {population}: the run never recorded {what:?}"
        );
    }
}

#[sqlx::test]
#[ignore = "requires PostgreSQL; set DATABASE_URL"]
async fn the_population_holds_all_four_invariants_under_crash_injection_first_seed(pool: PgPool) {
    simulate(pool, 0).await;
}

#[sqlx::test]
#[ignore = "requires PostgreSQL; set DATABASE_URL"]
async fn the_population_holds_all_four_invariants_under_crash_injection_second_seed(pool: PgPool) {
    simulate(pool, 1).await;
}
