//! The PostgreSQL runtime simulation (architecture section 18, layer 2 of the `engine` row).
//!
//! The real runtime runs on PostgreSQL: `flow.enroll`, `flow.step` and `webhook.call` jobs, with
//! event waits, over 90 virtual days and a few hundred enrollments on generated graphs that use
//! every node kind. Time is a `ManualClock`. Events arrive at seeded virtual times, many of them
//! at, just before or just after the timeout of the wait they answer. Every job is put through
//! seeded failures before its final successful run (see `inject`), and wakes are claimed before
//! they are due. After the run the four invariants are checked from the tables and the log of
//! the mock destination (see `invariants`).
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

#[sqlx::test]
#[ignore = "requires PostgreSQL; set DATABASE_URL"]
async fn the_population_holds_all_four_invariants_under_crash_injection(pool: PgPool) {
    let seeds = seeds(env::var(SEED_VAR).ok().as_deref());
    let population = population(env::var(POPULATION_VAR).ok().as_deref());
    for (ordinal, seed) in seeds.into_iter().enumerate() {
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
        println!(
            "seed {seed}: setup {built:.1?}, run {:.1?}",
            ran - built
        );
        for (what, count) in &driver.counts {
            println!("  {what}: {count}");
        }
        assert!(
            driver.problems.is_empty(),
            "simulation failed: seed {seed}, population {population} enrollments; rerun with \
             {SEED_VAR}={seed} {POPULATION_VAR}={population}\n{}",
            driver.problems.join("\n")
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
        ] {
            assert!(
                driver.counts.get(mode).copied().unwrap_or(0) > 0,
                "seed {seed}: the run never exercised {mode:?}"
            );
        }
    }
}
