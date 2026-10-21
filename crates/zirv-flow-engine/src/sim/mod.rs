//! The simulation's seeded building blocks: the SplitMix64 generator and the flow graph
//! generator. The engine's own harness (architecture section 18, layer 1) uses them in tests, and
//! the PostgreSQL simulation in `zirv-flow-jobs` (layer 2) reuses them through the `test-support`
//! feature, so both layers draw the same graphs from the same seeds. Nothing here is compiled
//! into a normal build.

pub mod generate;
#[cfg(test)]
mod harness;
pub mod rng;
