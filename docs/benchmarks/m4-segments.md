# M4 segment benchmark

Issue #127: architecture section 19 targets for segment preview, full refresh and incremental
re-evaluation at 10 million people and 50 million events, and the data for OQ-2. Measured with
published images: `ghcr.io/glubiz/zirv-flow:0.56.0` (main after #126), digest
`sha256:0b429e9f3dee4b652076429773539c10a39fc47121003c31f54cdb575039b7cd`,
`ghcr.io/glubiz/zirv-flow:0.57.0` (after #142 to #144), digest
`sha256:d99a14927195f59f547d771a9574e191d775d2c03741cf6617462d92bf37c65a` (section 6), and, for the
incremental phase only, `ghcr.io/glubiz/zirv-flow:0.57.1` (after #149), digest
`sha256:4ac6d44b751051b8c816f317565d11a2599736d3d5a8c6f13b8c4f9c764ad927` (section 6.6). The smoke run of the
scripts used 0.55.0 and is not a result. Raw JSON and plans are in
[`data/m4/`](data/m4/) (0.56.0), [`data/m4/0.57.0/`](data/m4/0.57.0/) and
[`data/m4/0.57.1/`](data/m4/0.57.1/); the scripts are in
`scripts/bench/m4/` (runbook: `scripts/bench/README.md` section 9).

**This is a development machine, not reference hardware.** Every figure is one host with Docker
Desktop. A target missed here is provisional (spec D1) and is not revised from this run alone.
Where a miss below is marked **code-caused**, a defect in the product's queries explains it, not the
hardware, and the re-check on reference hardware does not apply to it until the defect is fixed.

## 1. Summary

**Current state (0.57.1, sections 6 and 6.6).** The previews and refreshes were measured on 0.57.0 and the
incremental phase on 0.57.1, which changes only the re-evaluation batch statement. Every segment target of
section 19 is met, apart from D4.1 on `jsonb`, which was not re-run; the one miss is the ingestion API row
(an M2 target) below. Trait-only preview p95 1.06 s, aggregate preview p95 4.68 s,
property-filtered aggregate preview p95 1.20 s (no 503s), full refresh median 45.7 s trait-only and
14.5 s with an aggregate, event to `segment.entered` p95 4.05 s (target 60 s; p99 12.6 s, max 16.5 s,
17,475 entries), and all three API counts equal the hand-written SQL. **OQ-2 is decided: no rollup for
v1.** The 0.57.0 image missed D4.7 (p95 278.5 s): #143's per-execution custom plan of the
`segment.reevaluate` batch statement estimated 1 row for a segment missing from the `segment_members`
statistics and scanned every open row of the segment per batch person; #149 fixed it in 0.57.1. **One
section 19 target missed:** the ingestion API p99 (an M2 row, 100 ms) was 638.8 ms during the incremental
phase, against 60 ms on 0.56.0, and the cause is not established (section 6.5, #151). One caveat
stays: an aggregate preview right after an app restart took 15.1 s (a cold-cache observation, section
6.4). The rest of this section is the first run on 0.56.0.

As shipped means the 0.56.0 image with no change of settings. "Custom plan" is the same stack with
`ALTER DATABASE ... SET plan_cache_mode = force_custom_plan` (section 4.1), kept only as evidence of
what per-execution planning buys; the settings were reset afterwards.

| Target (section 19) | Measured as shipped | With `force_custom_plan` | |
|---|---|---|---|
| Preview, trait-only, p95 < 3 s, on `jsonb` (D4.1) | p50 5.52 s, p95 5.57 s (20 runs) | not run (the mirror statement counts in 1.55 s) | MISS, code-caused |
| Preview, trait-only, p95 < 3 s, on slots (D4.3) | p50 6.04 s, p95 6.14 s | p50 0.97 s, p95 1.01 s | MISS as shipped, code-caused; HIT with per-execution planning |
| Preview with a 90-day aggregate, p95 < 15 s (D4.4) | 20 of 20 previews 503 at the fixed 10 s limit; the query takes 38.7 s uncapped | 20 of 20 503; 37.7 s uncapped | MISS, code-caused |
| Preview, property-filtered aggregate, p95 < 15 s (D4.5) | 20 of 20 503 at 10 s; 97.5 s uncapped | 20 of 20 503; 98.0 s uncapped | MISS, code-caused |
| Full refresh, trait-only, < 2 min (D4.6) | median 77.6 s for the first refresh of a new segment (5 runs: 77.6, 78.6, 78.9, 40.0, 41.4); a manual refresh of it 5.4 s | not run | HIT |
| Full refresh with an aggregate, < 10 min (D4.6) | median 57.6 s first refresh (5 runs: 50.7 to 70.2); manual 45.0 s | not run | HIT |
| Event to `segment.entered`, p95 < 60 s (D4.7) | p95 0.382 s over 22,084 entries (p50 0.044 s, p99 9.7 s, max 13.3 s) at 2,000 events/s for 10 min | not run | HIT |
| Ingest cost of the change marks (D4.8) | drain medians 79,586 ms against 76,286 ms, +4.3%, interleaved on this machine (#122, PR #137); cited, not a new run | | cited |

Index backfills (D4.2, a data point, not a target) at 10 M people: `plan` 1,002.8 s, `age` 1,089.6 s,
`marketing_opt_in` 1,133.7 s, one after another (`trait_s1`, `trait_n1`, `trait_b1`).

## 2. Setup

### 2.1 Hardware (spec D1)

The reference hardware is PostgreSQL on 8 vCPU / 32 GB / NVMe and two app hosts of 4 vCPU. This run
used (`data/m4/stack.txt`):

- Host: 13th Gen Intel Core i9-13900K, Windows, Docker Desktop 29.2.1 (32 CPUs, 31 GiB for the VM),
  WSL 3.0.3.0.
- **PostgreSQL 16.15** in its own cpuset `8-31`, no memory limit, `shared_buffers=4GB`,
  `effective_cache_size=16GB`, `maintenance_work_mem=1GB`, `max_wal_size=8GB`,
  `checkpoint_timeout=15min`, everything else stock (`max_parallel_workers_per_gather=2`,
  `random_page_cost=4`). **`/dev/shm` is 8 GB** (`BENCH_PG_SHM=8gb`): the default 64 MB, and the
  1 GB first used, ran out during a parallel vacuum, so the container was recreated on the same
  volumes with 8 GB before any measurement.
- **zirv-flow at tier M**: cpuset `0-3` shared by serve, worker and scheduler, memory 3277 / 4096 /
  819 MB (serve / worker / scheduler), worker concurrency 4, `ingest.drain_shards=8`,
  `log.filter=warn`. loadgen runs on the host.
- Compose project `zirv-flow-m4bench10m`, image run with `--no-build` (never built locally).

### 2.2 Data (spec D3)

One workspace, written by SQL into the real tables after the app migrated the database
(`scripts/bench/m4/seed.sql`, `setseed(0.42)`), with the trait definitions and event schemas created
through the API. The stored shapes were checked against an identify and a track sent through the API.

| | Rows | Table | Indexes |
|---|---|---|---|
| `people` | 10,000,000 | 10.2 GB | 2.5 GB, of which the GIN `jsonb_path_ops` on `traits` 1.4 GB (slot indexes built later) |
| `identities` | 1,000,000 (the loadgen pool, `bench-u-0` to `bench-u-999999`) | 117 MB | 222 MB |
| `events` | 50,000,000 over 365 days, 0 in the default partition | 10.1 GB | 12.1 GB |

Events by name: `bench_page_viewed` 20,004,843, `bench_order_placed` 12,494,756,
`bench_item_added` 9,999,113, `bench_support_opened` 4,997,947, `bench_plan_changed` 2,503,341. Each
person has 20 mixed-type traits (the OQ-1 shape). 50 M events in 365 days is 10% of the 500 M
retained-event envelope of section 19. The seed ran from 04:52:13 UTC; the background command was
cut by a one-hour cap at 05:52 with every event committed (exact counts above), so the load took at
most 3,600 s; `VACUUM (ANALYZE, PARALLEL 0)` and the size manifest ran as separate commands.

**Deviations from the issue:** the event is `bench_order_placed`, not `order_placed`, because
`loadgen` hard-codes the five `bench_*` names and the incremental run needs its events to land in the
aggregate. The incremental segment uses the count aggregate: loadgen sends no `category`. The
loadgen pool is 1,000,000 people (`--people 1000000`).

### 2.3 Definitions (`scripts/bench/m4/defs/`)

- Trait-only: `plan eq pro` and `age between 25 and 45` and `marketing_opt_in eq true`: 677,521
  people. Its traits are the three indexed in D4.2.
- Aggregate: `plan eq pro` and `bench_order_placed` count >= 2 in the last 90 days (P90D): 128,399
  people at seed time.
- Property-filtered aggregate: sum of `amount` > 150.005 over `bench_order_placed` with
  `category eq books` in 90 days: 77,509 people.

The preview count of the trait-only definition equals a hand-written SQL count over the seeded rows
(677,521; `verify.json`). The two aggregate previews return 503, so their counts could not be compared
through the API at this size (the smoke run, 100,000 people, compared all three and they matched).

### 2.4 Method

Every measurement goes through the HTTP API as the bench owner. Preview: one warm-up and 20 timed
`POST .../segments/preview` calls (sample size 20, the default), p50 the median and p95 by nearest
rank, of curl's `time_total`. A preview that returns 503 is recorded with its status, and the statement is then
timed uncapped under `EXPLAIN (ANALYZE, BUFFERS)` through a hand-written mirror of the compiled SQL
(`explain.sql`; the store cannot print the compiler's SQL, so the mirror's count is checked against the
API's). Refresh: five fresh segments, each timed from the refresh job's `created_at` to the poll that
no longer finds the job (0.3 s poll, `total_s`), first (the save-triggered initial refresh, every
member enters) and manual (`POST .../refresh`, a full recompute that diffs against the open rows).
Incremental: a saved aggregate segment refreshed first, then `loadgen` at 2,000 track events/s for
600 s (1,199,999 accepted, 0 late, API p99 60 ms), until the inbox and dirty marks were empty, then
`incremental-latency.sql`: `decision_records.recorded_at` of each event-caused `segment.entered`
minus the causing event's `received_at` (the transaction start, a slight under-estimate).

## 3. Results

### 3.1 Trait-only preview and the slots

| Run | p50 | p95 | Notes |
|---|---|---|---|
| `jsonb` (D4.1) | 5.52 s | 5.57 s | `min` 1.58 s. The mirror statement runs in 1.55 s (parallel scan) |
| Slots right after `ready`, before `ANALYZE` | 6.40 s | 6.87 s | index scan on `people_trait_b1_idx`, row estimate 1 (actual 677,521); 3.8 s for the mirror |
| Slots after `ANALYZE people` (2 s), autovacuum still running | 5.93 s | 6.31 s | the mirror now plans a parallel seq scan, 1.07 s |
| Slots, quiet database (headline) | 6.04 s | 6.14 s | |
| Slots, `force_custom_plan` | 0.97 s | 1.01 s | |

The slot indexes are used (`explain_indexes` and `*.explain.txt`), but they are not what decides the
time. Two things were found while explaining why the API is five seconds slower than the same count
run by hand (`preview-slots.api-sql.explain.txt`, `preview-slots.api-statements.log`):

1. **Generic plan.** The API runs the compiled count as a persistent prepared statement (sqlx). After
   five executions on a connection PostgreSQL switches to a generic plan, which cannot see the bound
   values and so estimates 7,362 rows for the 677,521 that match. It picks a non-parallel index scan
   on one slot (5.9 to 6.2 s) instead of the parallel sequential scan of the first five executions
   (1.0 s). Reproduced in psql with the logged statement text: executions 1 to 5 run the parallel
   plan, the sixth the index scan. The same flip is why `jsonb` is bimodal (min 1.58 s, p50 5.5 s). At
   the 100,000-person smoke size it is invisible. **Code-caused.**
2. **Stale statistics.** The backfill does not `ANALYZE`, so right after `ready` the new slot columns
   have no statistics (`trait_b1`: all null) and the estimate is 1 row. `ANALYZE people` takes 2 s at 10 M
   rows. After it the custom plan is correct; the generic plan is not, so the two fixes are needed
   together. The mid-backfill autovacuum of `people` also ran for several minutes afterwards and slows
   previews while it does; the headline is from a quiet database. **Code-caused.**

The 20-row sample statement takes 0.3 ms and is not part of the gap.

### 3.2 Aggregate previews and OQ-2

Both aggregate previews are 503 (`segment_preview_timeout`) on 20 of 20 runs, as shipped and with
`force_custom_plan`. The API's statement timeout is a fixed 10 s (`PREVIEW_TIMEOUT`, no setting), so
the 15 s target cannot be met by any query slower than 10 s; the uncapped times are the measured
misses:

| Definition | Uncapped, as shipped | Uncapped, custom plan | Target |
|---|---|---|---|
| Aggregate (D4.4) | 38.7 s | 37.7 s | 15 s |
| Property-filtered aggregate (D4.5) | 97.5 s | 98.0 s | 15 s |

The plan is a correlated scalar subquery per candidate person (`COALESCE((SELECT count(*) FROM events e
WHERE e.person_id = p.id AND ...) >= 2, false)`), which PostgreSQL does not decorrelate: a SubPlan runs
once per person that survives the cheaper predicates (a probe of the four partitions that cover 90
days each time), and for the sum with no other trait condition it runs for every one of the 10 M
people. Neither the plan cache mode nor the slots change that. The full refresh runs the same SQL with
no 10 s limit, which is why it takes 45 s (aggregate) while the preview 503s.

### 3.3 Refresh and incremental

The trait-only first refresh (every one of 677,521 members enters, with an event and a decision row
each) takes 77.6 s at the median, inside 2 min; runs 4 and 5 took about 40 s for a reason not
identified here (possibly a plan difference on the worker's connection). A manual refresh, where
nothing changes, is 5.4 s. The aggregate refresh is 57.6 s at the median (the compile plus the 45 s
query), inside 10 min. Incremental: 22,084 event-caused entries into the aggregate segment (128,230
members before, 150,313 after, 1 exit), p95 0.382 s, p99 9.7 s, max 13.3 s, target 60 s. The inbox and
the marks were empty after the drain wait that follows the loadgen run. The refresh and re-evaluation jobs were measured as
shipped only.

## 4. Decisions and follow-ups

### 4.1 OQ-2: decided, no rollup for v1

Decided on the 0.57.0 re-run (section 6) by #127's D5 rule: both aggregate previews meet p95 < 15 s
(D4.4 4.68 s, D4.5 1.20 s, no 503s) and the aggregate refresh meets < 10 min (median 14.5 s). **No
rollup for v1; event aggregates compile to grouped sets (#144).** On 0.56.0 the same preview took 38.7 s
and the property-filtered one 97.5 s uncapped (503 at the 10 s limit), from the compiler's correlated
subquery per person, not from the absence of a rollup (supervisor ruling b54f3196 kept OQ-2 open until
the compiler was reshaped and re-benched at the same 10 M / 50 M). The margin on the aggregate preview
is thin on a cold cache (section 6.4): to be re-checked on reference hardware at M8, where a rollup
returns only if the warm-cache preview misses 15 s.

### 4.2 Missed targets

On 0.57.1 (sections 6 and 6.6) the misses of 0.56.0 and 0.57.0 that were re-measured are closed:

| Miss | Gap | Cause | Closed by |
|---|---|---|---|
| Trait-only preview on `jsonb` and on slots (0.56.0) | 5.57 s and 6.14 s against 3 s | generic plan of the persistent prepared statement; no `ANALYZE` after a backfill | #143 (slots: 1.06 s on 0.57.0; `jsonb` not re-run) |
| Aggregate previews (0.56.0) | 38.7 s and 97.5 s uncapped; 503 at 10 s | correlated subquery per person | #144 (4.68 s and 1.20 s) |
| 10 s limit against the 15 s target (0.56.0) | 503 even at 10 to 15 s | design inconsistency | #142 |
| Event to `segment.entered` (0.57.0) | p95 278.5 s against 60 s | #143's custom plan of the batch statement, with the incremental segment missing from the `segment_members` statistics (follow-up D) | #149 (p95 4.05 s on 0.57.1) |

What stays open: the ingestion API p99 missed 100 ms during the incremental phase (638.8 ms; cause not
established, section 6.5, #151), D4.1 on `jsonb` was not re-run, an aggregate preview on a cold cache can sit
at the 15 s target (section 6.4), and everything remains provisional under D1 (hardware pending M8). Section 19's
fallbacks (name-level dirty filtering, `database.read_url`, rollups) are not needed.

### 4.3 Follow-up issues

**A (#143). Segment queries lose their parallel plan after five executions (generic plan), and slot backfills leave no statistics**

At 10 M people the slotted trait-only preview takes p95 6.14 s as shipped (target 3 s) and 1.01 s with
`plan_cache_mode = force_custom_plan` (D4.3). The compiled count is a persistent sqlx prepared
statement; after about five executions on a connection PostgreSQL uses a generic plan that estimates
7,362 rows for 677,521 and runs a non-parallel index scan (5.9 s) instead of the parallel sequential
scan (1.0 s). The `jsonb` preview is bimodal for the same reason (min 1.58 s, p50 5.52 s). Separately,
a slot backfill does not `ANALYZE`, so the new columns have no statistics (estimate 1 row) until
autovacuum. Fix: `.persistent(false)` on the compiled segment count, sample and membership queries
only (not `plan_cache_mode` in `begin_snapshot_for`), and `ANALYZE people` when a backfill reaches
`ready`. Check whether the refresh and re-evaluation queries share it: the refresh runs once per job,
and the trait-only first refresh varied between 78 s and 40 s across runs (unexplained). Evidence:
`docs/benchmarks/data/m4/preview-slots*.json`, `preview-slots.api-sql.explain.txt`.

**B (#144). Segment aggregate predicates are a correlated subquery per person (OQ-2)**

Compile event aggregates as a grouped set over events (workspace, name, window, property filters,
`GROUP BY person_id HAVING agg op N`) semi-joined to `people`, with a `LEFT JOIN`/`COALESCE` form for
comparisons that zero-event people satisfy. At 10 M people / 50 M events a 90-day count aggregate takes
38.7 s and a property-filtered sum 97.5 s uncapped (targets 15 s; both 503 at the 10 s limit), and the
refresh of the aggregate segment takes 45 s per full recompute. Re-bench at the same size; decide
"no rollup for v1" only if both cases meet 15 s, otherwise design a rollup that also covers
property-filtered aggregates.

**C (#142). The 10 s preview timeout contradicts the 15 s aggregate target (architecture 8.2 against 19)**

`PREVIEW_TIMEOUT` is a fixed 10 s (`crates/zirv-flow-api/src/segments/mod.rs:20`), with no setting.
Meeting 15 s alone still leaves 503s for previews between 10 and 15 s. Ruling 9201125b: raise it to a
fixed 20 s, a margin above the target, with no config key, and say so in 8.2; the section 19 targets
stay.

**D (#149). The incremental re-evaluation batch statement gets a bad custom plan for a segment missing from the statistics**

On 0.57.0 at 10 M people with an aggregate segment of about 150,000 members, 2,000 track events/s for
600 s gave event to `segment.entered` p95 278.5 s (p50 140.5 s, max 284.7 s; 46,107 entries) against
60 s (0.38 s on 0.56.0). The cause is verified, and it is not the aggregate compile of #144: #143 made the
`segment.reevaluate` batch statement `.persistent(false)`, so each batch is planned with the segment id as a
literal. A new segment is missing from the statistics of `segment_members` (on the 0.57.0 database, read
while diagnosing and recorded in #149: 8.7 M rows, 19 most common values summing to 1.0, and the autoanalyze
threshold of about 871 k changes not reached; the 0.57.1 state is in `data/m4/0.57.1/incremental-stats.txt`), so the planner
estimates 1 row and scans every open row of the segment for each batch person. For the same 23-person batch on
the same database, with the segment still missing from the statistics, the custom plan took 624 and 716 ms and
the generic plan 0.89 and 0.83 ms, probing `(segment_id, person_id)`
(`data/m4/0.57.1/incremental-batch.custom-vs-generic.txt`); a `LATERAL ... LIMIT 1` rewrite tried during the
diagnosis did not help (273 ms, #149). Batches are serialized by the segment's lock, so one segment's
throughput fell below the arrival rate and the backlog grew. 0.56.0 was fast because its persistent
statements switched to the generic plan after five executions. Fix (#149, ruling d30289c3):
`apply_reevaluation` runs the batch statement under `SET LOCAL plan_cache_mode = force_generic_plan` and
resets it right after. Shipped in 0.57.1: p95 4.05 s (section 6.6).

## 5. Reproduce

```sh
docker pull ghcr.io/glubiz/zirv-flow:0.56.0
cargo build --release -p zirv-flow-loadgen
export M4_PROJECT=zirv-flow-m4bench10m BENCH_IMAGE_TAG=0.56.0 BENCH_PG_SHM=8gb
export SEED_PEOPLE=10000000 SEED_EVENTS=50000000 SEED_LG_POOL=1000000 M4_INC_POOL=1000000
scripts/bench/m4/stack.sh up && scripts/bench/m4/setup.sh
SEED_STEPS=load scripts/bench/m4/seed.sh                    # about an hour; keep a command under the host's cap
SEED_AT=<seed_at of the load> SEED_STEPS="vacuum-people vacuum-identities" scripts/bench/m4/seed.sh
SEED_AT=<seed_at> SEED_STEPS=vacuum-events scripts/bench/m4/seed.sh
SEED_AT=<seed_at> SEED_STEPS=manifest SEED_DURATION_S=<s> scripts/bench/m4/seed.sh
for p in verify preview-jsonb index preview-slots preview-agg preview-sum refresh-trait refresh-agg incremental; do
  scripts/bench/m4/measure.sh $p; done
```

For the custom-plan set: `ALTER DATABASE zirv_flow SET plan_cache_mode = force_custom_plan`, recreate the
`zirv-flow` service so the pool reconnects, measure, then `ALTER DATABASE zirv_flow RESET plan_cache_mode`.

## 6. Re-run on 0.57.0 (after #142 to #144)

### 6.1 Image and method

Image `ghcr.io/glubiz/zirv-flow:0.57.0`, digest
`sha256:d99a14927195f59f547d771a9574e191d775d2c03741cf6617462d92bf37c65a` (created 2026-10-18 10:20 UTC),
containing #142 (20 s preview limit), #143 (per-execution planning, `ANALYZE` after a backfill) and #144
(grouped aggregates). The same stack, volumes and 10 M / 50 M seed as section 2 (compose project
`zirv-flow-m4bench10m`), brought up on the new image; 0.57.0 adds no migration and none ran. Three
slot-backed traits `ready`, 10,000,000 people. `events` now holds about 52.4 M rows: the 0.56.0
incremental run left 1.2 M new events and this run added 1.2 M, in the newest partition, so the
aggregate counts are not the seed-time counts. Method as section 2.4, one phase at a time with nothing
else heavy running, plus one plain `ANALYZE people` and `ANALYZE events` (9 s) before the previews. The
`index` phase was skipped: the slots are already `ready`, and the `ANALYZE` after a backfill is covered by
#143's test. Consequence: the "jsonb" phase (D4.1) now runs the same slot-backed statement as the slots
phase, so D4.1 on `jsonb` is not re-measured (the 0.56.0 figure stands for the jsonb shape). Results are in
[`data/m4/0.57.0/`](data/m4/0.57.0/) under the names of the 0.56.0 files.

### 6.2 Results against the section 19 targets

| Target (section 19) | 0.56.0 | 0.57.0 | |
|---|---|---|---|
| Preview, trait-only, p95 < 3 s, on `jsonb` (D4.1) | p95 5.57 s | not re-measured (slot-backed statement: p50 1.00 s, p95 1.05 s) | open, jsonb shape not re-run |
| Preview, trait-only, p95 < 3 s, on slots (D4.3) | p95 6.14 s | p50 1.00 s, p95 1.06 s, 0 non-200 | HIT |
| Preview with a 90-day aggregate, p95 < 15 s (D4.4) | 20 of 20 503; 38.7 s uncapped | p50 4.46 s, p95 4.68 s, 0 non-200 (min 4.33, max 5.02) | HIT |
| Preview, property-filtered aggregate, p95 < 15 s (D4.5) | 20 of 20 503; 97.5 s uncapped | p50 1.14 s, p95 1.20 s, 0 non-200 | HIT |
| Full refresh, trait-only, < 2 min (D4.6) | median 77.6 s | median 45.7 s (53.4, 43.8, 45.1, 45.7, 49.7); manual 6.0 s | HIT |
| Full refresh with an aggregate, < 10 min (D4.6) | median 57.6 s | median 14.5 s (19.7, 14.5, 14.1, 14.2, 14.5); manual 8.2 s | HIT |
| Event to `segment.entered`, p95 < 60 s (D4.7) | p95 0.382 s | 0.57.0: p95 278.5 s (46,107 entries), MISS. 0.57.1: p50 0.043 s, p95 4.05 s, p99 12.6 s, max 16.5 s (17,475 entries), section 6.6 | HIT on 0.57.1 |

The incremental run was the same as before: 2,000 events/s for 600 s (1,200,000 accepted, 0 late, 0 429), an
aggregate segment of 149,976 members before and 196,083 after, 0 exits, nothing pending after the drain.
The ingest API p99 was 299 ms (60 ms on 0.56.0).

**Verify (spec D3).** The preview count through the API equals the hand-written SQL count of
`verify.sql` for all three definitions: trait-only 677,521 = 677,521; aggregate 150,009 = 150,009;
property-filtered aggregate 77,361 = 77,361 (each at its own `compiled_at`). On 0.56.0 the two aggregate
previews returned 503, so this is the first comparison of them at 10 M. The counts of the timed runs
vary slightly (150,002 to 150,006), which the sliding 90-day window explains; the mirror and API
counts agree at each run's last `compiled_at`.

### 6.3 Plans

The plans are of the statement the API actually runs, not a hand-written mirror: `log_min_duration_statement=0`
was set on the bench database for one call per definition, the logged SQL was run with its values under
`EXPLAIN (ANALYZE, BUFFERS)` through psql's `\bind` (an unnamed statement, planned per execution as the API
now does), and the setting was reset
(`data/m4/0.57.0/preview-*.api-sql.explain.txt`, `preview.api-statements.log`; `preview-slots.explain.txt` and
`preview-jsonb.explain.txt` are the older mirror of the trait-only statement, still the same shape).

- **Trait-only** (slots): `Parallel Seq Scan on people` with two workers, 1.1 s warm (3.9 s first and 1.5 s on the repeat in the
  captures). No generic-plan flip: every execution is planned for its values.
- **Aggregate (D4.4):** `people` is probed 589,985 times through `people_pkey` after a parallel scan of the
  four partitions that cover 90 days, a partial hash aggregate and a sort/group; the planner estimates 176,085
  groups and 58,777 result rows against 589,985 and 196,055. One execution: 8.7 s (9.9 s first) in the capture,
  about 4.5 s in the timed runs. The scan reads about 3.5 M event rows per execution.
- **Property-filtered sum (D4.5):** the `properties @> {"category":"books"}` filter and the sum run in the same grouped scan, 304,918
  groups before the `HAVING`, 77,331 after; 1.5 s warm (2.4 s first).

### 6.4 Cold-cache caveat (one observation, not a target)

The first preview of each aggregate is a warm-up, recorded apart from the 20 timed runs. In the
normal run the aggregate warm-up took 4.52 s against a p50 of 4.46 s, so it was not slower; the
property-filtered sum took 2.79 s against 1.14 s (2.4 times). Separately, about four minutes after the
incremental phase had written 1.2 M events, and right after the app restart that turned logging on, the same previews took
3.0 s (trait-only count 2.97 s), 15.1 s (aggregate; its count statement 13.6 s) and 4.6 s (sum), and the
immediate repeats under `EXPLAIN` 1.5 s, 8.7 s and 1.5 s with the same plans. The page cache was not
evicted on purpose. So the aggregate preview is 4.5 s only when the 90-day window of `events` is cached:
the grouped set reads about 3.5 M rows and the `people_pkey` probes depend on how well the planner's
too-low group estimate (176 k against 590 k) holds. A cold first run can sit at the 15 s target, so this is
the figure to re-check on reference hardware at M8. The orchestrator's earlier cold measurement of the D4.4 statement, 53.7 s after
an hour of builds had evicted the cache, is consistent with this and not reproduced here.

### 6.5 What still misses

- **Event to `segment.entered`** missed on 0.57.0 (p95 278.5 s against 60 s) and is met on 0.57.1 (p95 4.05 s, section 6.6); the cause and the
  fix (#149) are in section 4.3 D. The generic plan of a 23-person batch is in
  `data/m4/0.57.1/incremental-batch.explain.txt` (the 0.57.0 custom plan: `data/m4/0.57.0/`).
- **Ingestion API p99: missed** (section 19, an M2 row: p99 < 100 ms at the sustained rate). At 2,000 events/s
  it was 60 ms on 0.56.0, 299 ms on 0.57.0 (during the backlog), 464.9 ms on a 0.57.1 run with no membership
  changes (seed 1) and 638.8 ms on the 0.57.1 run of 6.6 (p50 4.0 ms, p95 131.6 ms). The time is spent on the
  server (`service_latency` p99 635.3 ms; loadgen dispatch lag p99 7.6 ms). No ingest, inbox or drain code
  changed between 0.56.0 and 0.57.1, and every run used the same database, which gains a segment and 1.2
  million events per run. The version A/B that #151 asked for is no longer possible (6.5.1); a factor A/B on
  0.76.0 shows that segment re-evaluation roughly doubles the p99, and that with no active segment it still
  sits at 640 to 740 ms. The cause of that baseline is not established (6.5.1).
- D4.1 on `jsonb` was not re-measured (6.1).
- Everything remains provisional under D1 until the M8 hardware run.

#### 6.5.1 Ingest p99 with and without segment re-evaluation, on 0.76.0 (#151)

This tests the overhead of segment re-evaluation on 0.76.0. It is **not** the 0.56.0 to 0.57.1 regression:
the M5 timer bench (#169) ran image 0.76.0 on this stack and migrated the database through
`0036_flow_runtime`; zirv-flow refuses to start on a newer schema (`SchemaState::Newer`), and no backup from
before that migration exists, so 0.56.0 and 0.57.1 cannot be run on this database any more. Any
version-specific attribution is carried to the M8 hardware run (`docs/roadmap.md`, M8 Performance).

Setup. Image `ghcr.io/glubiz/zirv-flow:0.76.0` (digest in `data/m4/151-ab/stack.txt`), compose project
`zirv-flow-m4bench10m`, app tier M, PostgreSQL 16.15 with `/dev/shm` 8 GB (`BENCH_PG_SHM=8gb`; the M5 bench
had 1 GB, so the PostgreSQL container was recreated on the same volumes), `max_wal_size` 8 GB,
`synchronous_commit` on. Before the first run the workspace held 25 archived segments, none active, and
9,117,293 `segment_members` rows. Its three M5 flows (`m5-spread`, `m5-burst`, `m5-probe`) are published
with a manual trigger, so nothing reacts to track events or to segment entries; none was changed. Six dated
`flow.step` wake-up jobs (tomorrow and 2099) were the only jobs in the queue. Two 30 s smoke runs (seeds 9001
and 9002, not in the table) preceded the runs. Each run: 2,000 track events/s for 600 s over 1,000,000
people (`scripts/bench/m4/ab.sh`), a fresh seed, no `cargo.exe` running, and the inbox, the dirty marks and
the due jobs drained first.

- **A**: a fresh `agg90` segment is saved and refreshed (230 k to 270 k members), then the load runs.
- **B**: no active segment in the workspace.

| Run | Seed | `service_latency` p50 / p95 / p99 (ms) | From schedule p99 (ms) | Dispatch lag p99 (ms) | PostgreSQL / serve / worker CPU (% of a core, mean) | Jobs enqueued per s | WAL (GB) |
|---|---|---|---|---|---|---|---|
| A1 | 1101 | 3.4 / 697.5 / 1345.5 | 1350.6 | 9.4 | 422 / 128 / 107 | 69.2 | 17.4 |
| B1 | 1102 | 3.5 / 185.5 / 742.4 | 746.4 | 7.6 | 440 / 130 / 104 | 81.9 | 17.6 |
| A2 | 1103 | 3.5 / 1043.8 / 1713.1 | 1716.1 | 10.7 | 417 / 132 / 105 | 77.0 | 19.1 |
| B2 | 1104 | 3.4 / 143.4 / 638.1 | 642.9 | 7.5 | 401 / 127 / 105 | 112.5 | 19.1 |
| B3 (wait-event sampler on) | 1105 | 3.5 / 142.9 / 707.3 | 711.6 | 7.7 | 447 / 130 / 101 | 85.3 | 20.4 |

Every run accepted all of its events (about 1,200,000; no 429, no 5xx, no late dispatch). The event to
`segment.entered` p95 in A was 1.72 s and 0.32 s (19,438 and 20,489 entries).

What the runs show.

- Segment re-evaluation matters. The A p99 is 1.35 and 1.71 s against 0.64 to 0.74 s for B, and the A p95 is
  700 to 1,040 ms against 143 to 186 ms (3.7 to 7.3 times, pairwise). The order held in every pair and the spread within a condition is
  narrow next to the gap.
- It is not the whole story. B has no active segment and still misses 100 ms by 6 to 7 times, and sits at or
  above the 635 ms of 0.57.1 in 6.6. The p50 (3.4 to 3.5 ms) is the same in every run.
- Condition B still runs the re-evaluation machinery. Every accepted event batch marks its people in
  `person_changes` and requests a (deduplicated) `segment.reevaluate` job (`request_reevaluation`, called unconditionally at `apply.rs:86`, whether or not a segment
  exists), so the job is claimed in B as well: a running `segment.reevaluate` job was seen in 82 to 101 of
  about 203 one-second queue samples, against 103 to 110 in A. In B it claims the marks and finds no segment
  to evaluate. So the check "no `segment.reevaluate` job is claimed in B" does not hold; what holds is "no
  segment is evaluated in B" (`members_ins` and `members_upd` are 0).
- PostgreSQL and app CPU are the same in both conditions, so the extra latency of A is not CPU. The WAL
  volume is also the same: 17 to 20 GB per 600 s run, 14 to 17 KB per event, 4 to 5 checkpoints, against
  `max_wal_size` 8 GB.
- Wait events of the client backends, sampled once per second during B3 only (the sampler overhead was not measured and its own queries appear in the samples; waits were not sampled in A, so they say nothing about what A adds; 3,747 samples,
  `data/m4/151-ab/diag-waits.csv`): 43 % on CPU, 31 % `LWLock:WALWrite` and 11 % `IO:WALSync`, nearly all on
  `COMMIT`; 5 % `Lock:transactionid`, mostly on the `INSERT INTO jobs` that enqueues the workspace's single
  `segment.reevaluate:<workspace>` unique key. So the commits of the ingest path queue behind the WAL flush.
  Over the life of the database: 6.1 M `wal_buffers_full` (16 MB `wal_buffers`), 91 requested against 16
  timed checkpoints, `wal_compression` off.

Verdict. Re-evaluation explains part of the rise (about 0.6 to 1 s of p99 on this database) but not the
baseline. The ingest path is dominated by WAL flush contention at 17 to 20 GB of WAL per 10 minutes. The
mechanism by which an active segment adds to that is not established. Two candidates are left for a reviewed
follow-up (#209): the hot unique key that every ingest transaction contends on, and the write volume per event
(an `events` row, a `decision_records` row, a dedupe row and a mark). Neither was tested, and no code was
changed. Not covered: the 0.56.0 to 0.57.1 attribution, a run with `synchronous_commit` off or a larger
`wal_buffers`, and a reference-hardware run.

Drift between runs. Each A run adds about 250 k `segment_members` rows (archived afterwards), the order was A B A B B so B3 ran on the most-grown database and is not alternated, and A1 followed a recreation of the PostgreSQL container. None of this is controlled for.

Data: [`data/m4/151-ab/`](data/m4/151-ab/): per run `ab-<condition>-<seed>.json` with its loadgen report,
the CPU and queue summaries (from the per-3 s `docker stats` and per-2 s queue samples, not kept) and the counters of the window.

### 6.6 Incremental on 0.57.1 (after #149)

Image `ghcr.io/glubiz/zirv-flow:0.57.1`, digest
`sha256:4ac6d44b751051b8c816f317565d11a2599736d3d5a8c6f13b8c4f9c764ad927`, the same stack, volumes and 10 M
seed, only the `incremental` phase (data in [`data/m4/0.57.1/`](data/m4/0.57.1/)).

A first run with the default loadgen seed produced no entries at all: the seed fixes the items and the people
they go to, so after two earlier runs on the same pool every person who could enter already had (that run is
kept as `incremental-seed1-no-entries.json`). `measure.sh` now takes `M4_INC_SEED`; the result below is
`M4_INC_SEED=2`, which sends different items to the same 1,000,000 people.

| | 0.56.0 | 0.57.0 | 0.57.1 |
|---|---|---|---|
| Entries caused by events | 22,084 | 46,107 | 17,475 (2 exits) |
| p50 | 0.044 s | 140.5 s | 0.043 s |
| p95 (target 60 s) | 0.382 s | 278.5 s | **4.05 s** |
| p99 | 9.7 s | 282.9 s | 12.6 s |
| max | 13.3 s | 284.7 s | 16.5 s |

2,000 events/s for 600 s: 1,200,000 accepted, 0 late; the segment went from 195,547 to 213,020 members and
nothing was pending after the drain. Ingestion API p99 638.8 ms against 100 ms, a miss (p50 4.0 ms, p95
131.6 ms; section 6.5, #151).

**Statistics state.** After the run the incremental segment is not in `pg_stats.most_common_vals` for
`segment_members.segment_id` (24 listed values). `segment_members` was last auto-analyzed at 15:24:45 UTC, before
the segment was created at 15:37:52, with 213,024 changes since the last analyze (`n_live_tup` 9,116,705) and
no manual analyze. So the statistics were stale for this segment during the whole run, as on 0.57.0, and the
fast result comes from the plan, not from the statistics.

**Plan.** For a 23-person batch of the incremental segment, run once in a rolled-back transaction under
`SET LOCAL plan_cache_mode = force_generic_plan` (as `apply_reevaluation` does; `PREPARE` and `EXPLAIN EXECUTE`,
because `EXPLAIN` of a bound statement is always a custom plan): execution 2.8 ms, planning 5.7 ms. Both
segment membership probes use `segment_members_open_idx` with `Index Cond: (segment_id = $8) AND (person_id = t.id)`
and `(segment_id = $8) AND (person_id = ANY ($7))`, 23 rows. On 0.57.0 the same statement took 390 ms
because the custom plan scanned about 94,000 open rows of the segment per batch person
(`data/m4/0.57.1/incremental-batch.explain.txt`, `data/m4/0.57.0/incremental-batch.explain.txt`). The same
batch on the 0.57.1 database took 624 and 716 ms under a custom plan and 0.89 and 0.83 ms under the generic
plan (`data/m4/0.57.1/incremental-batch.custom-vs-generic.txt`).
