# Plan: #156

1. One worker, from `.zirv/work/m5-plan/impl156-brief.md` in the main repository:
   - creates `crates/zirv-flow-engine` (the workspace glob includes it; the crate-edge allowlist already permits engine to core);
   - writes the types and `step`;
   - writes the inline tests the issue lists, plus a fixed-seed property test.
2. The checks: build, nextest (engine and core), fmt, clippy, the crate-edge check and the test-presence check.
3. A simplify pass, then independent Sonnet and Codex reviews. At most 2 fix rounds.
4. The version bump at integration, then the PR.
