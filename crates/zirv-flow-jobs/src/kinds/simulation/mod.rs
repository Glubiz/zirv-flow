//! The PostgreSQL runtime simulation (architecture section 18, layer 2 of the `engine` row).
//!
//! The real runtime runs on PostgreSQL: `flow.enroll`, `flow.step` and `webhook.call` jobs, with
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
    for mode in [
        "rollback flow.step",
        "commit without ack flow.step",
        "concurrent duplicate flow.step",
        "rollback webhook.call",
        "commit without ack webhook.call",
        "concurrent duplicate webhook.call",
        "rollback flow.enroll",
        "commit without ack flow.enroll",
        "concurrent duplicate flow.enroll",
        "early wake claimed",
        "early wake put back",
        "wake moved an enrollment",
        // The concurrent duplicates overlap for real, and each variant shows it: the fenced
        // claim was skipped while A was open, the unfenced B was blocked on A, and A's ack was
        // refused every way. A webhook B never waits on A, so only its finish is counted.
        "fenced: A open while B claimed flow.step",
        "fenced: claim skipped flow.step",
        "fenced: claim skipped webhook.call",
        "fenced: A's ack refused flow.step",
        "fenced: A's ack refused webhook.call",
        "unfenced: B blocked on A flow.step",
        "unfenced: B finished without waiting on A webhook.call",
        "unfenced: A's ack refused flow.step",
        "unfenced: A's ack refused webhook.call",
        // The destination fails too: calls are retried, some use up their attempts through
        // lease expiry, some record a failure on their last attempt, and the outcome events a
        // call writes twice settle each wait once.
        "webhook.call attempts retried",
        "webhook.call jobs dead",
        "waits on a webhook outcome",
        "outcome events that settled a wait",
        "outcome events that settled nothing",
    ] {
        assert!(
            driver.counts.get(mode).copied().unwrap_or(0) > 0,
            "seed {seed}: the run never exercised {mode:?}"
        );
    }
    for what in [
        "webhook outcomes: succeeded",
        "webhook outcomes: failed on the last attempt",
    ] {
        assert!(
            found.summary.get(what).copied().unwrap_or(0) > 0,
            "seed {seed}: the run never recorded {what:?}"
        );
    }
    // The issue asks for a failure after every effect: no job runs once without one.
    let count = |what: &str| driver.counts.get(what).copied().unwrap_or(0);
    println!(
        "seed {seed}: {} of {} job executions were injected before their final run",
        count("injected executions"),
        count("executions")
    );
    assert!(
        count("executions") > 0 && count("injected executions") == count("executions"),
        "seed {seed}: {} of {} job executions were injected",
        count("injected executions"),
        count("executions")
    );
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
