## Memory
- Key: stale-stats-in-flight-plans
- Written-by: claude
- Written: 1795689265
- Verified: 1795689265
- Source: explicit

`enrollments` is often analyzed while nothing is in flight (a pause between bench steps, a quiet install). The stats then read `state` as 100 % completed, and the planner treats the in-flight partial indexes as empty. A full scan of an index believed empty looks cheaper than any probe, so set-wide joins and `= ANY` lists over in-flight rows cost O(in-flight) per call until the next analyze. That was the tier L stalls of 45 to 60 s, fixed in PR #874 (0116). Verified on PostgreSQL 16.16 and 18.6.

Write hot in-flight lookups as fenced per-key LATERAL probes (`OFFSET 0` or `LIMIT 1`) on an index whose leading column is the probe key. Test them with `seed_stale_in_flight` and `plan_buffers` (`flows/mod.rs` `tests_support`) under both `force_generic_plan` and `force_custom_plan`. Seed in random order: rows inserted contiguously sit on a few heap pages and hide the cost.

`ALTER COLUMN ... SET STATISTICS 0` does not clear existing stats; only a same-type `ALTER COLUMN TYPE` does, and that takes ACCESS EXCLUSIVE and rebuilds the table's partial indexes.

sqlx 0.9 `PoolOptions::connect` acquires the first connection within `acquire_timeout` (pool/options.rs:541), retrying ConnectionRefused and startup errors only until then. A short acquire timeout therefore also shortens startup tolerance, which is why `zirv_flow_store::connect` retries for `STARTUP_WAIT`.
