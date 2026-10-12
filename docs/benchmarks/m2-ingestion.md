# M2 ingestion benchmark

Issue #47: architecture section 19 targets for the M2 write paths (ingestion over HTTP, the durable
inbox and its drain, the decision log) and the OQ-1 trait storage decision. Measured on `main` at
1490f8d (version 0.30.0 adds only this tooling), with the drain fix of PR #73 (`ingest.drain_shards`,
default 8) and, for the before column, the drain as it was at 0.28.0.

**This is a development machine, not reference hardware.** Every figure below is a measurement on one
host with Docker Desktop. The section 19 targets are re-checked on reference hardware before 1.0; nothing
here changes a target.

## 1. Summary

| Target (section 19) | Measured | |
|---|---|---|
| Sustained ingestion, 2,000 items/s | 2,000/s for 10 min on one workspace in tiers S, M and L, no errors, generator on schedule | HIT |
| Burst 10,000/s for 10 min, backlog drained within 15 min | 6 million items in 10 min, backlog drained 890 s (14.8 min) after the generator stopped, with each drain shard on its own task (section 5.1). Before: 2-minute bursts drained 237 to 243 s after stopping and the 10-minute case projected to about 19 min | HIT, by 10 s (issue: #74) |
| Ingestion API p99 under 100 ms at the sustained rate | 80.2 ms (S), 11.0 ms (M), 12.7 ms (L), judged on the schedule-based latency | HIT |
| Inbox to applied profile p95 under 5 s at the sustained rate | 45.8 ms (S), 38.1 ms (M), 35.1 ms (L) | HIT |
| Decision-log append: added transaction time p95 under 5 ms | 52.9 ms per 500-event drain transaction in the container network (184.5 ms from the host); about 0.1 ms per decision | MISS (issue: #75) |
| Decision-log append: throughput loss under 10% | 34.4% (drain 1,065 items/s with decisions, 1,623 without) | MISS (issue: #75) |

Also missed, outside section 19's rows but against its "API p99 under 100 ms at the sustained rate" for the
500-item batch endpoint: API p99 124.9 ms at 2,000 items/s in batches of 500 (tier M)
(issue: #76).

Before the drain fix one workspace drained about 900 items/s; a 10-minute run at 2,000/s ended with a
backlog that took about 11 minutes to apply after the generator stopped (inbox-to-applied p95 of 640 s). After it, p95 is below 50 ms and
the drain finishes within 13 s of the generator stopping (section 5).

## 2. Method

### 2.1 Stack and tiers

The load generator (`tools/loadgen`) runs on the host, outside every budget, and talks to a published
port. The system under test runs in Docker Compose (`docker-compose.yml`, `scripts/bench/compose.bench.yaml`
and `scripts/bench/compose.tiers.yaml`), from a release image built from this branch:

- **zirv-flow** (separate `serve`, `worker` and `scheduler` services) shares one cpuset and memory
  budget per tier, like one VM of that size, with worker concurrency 4 in every tier:

  | Tier | vCPU (cpuset) | RAM | worker / serve / scheduler memory |
  |---|---|---|---|
  | S | 2 (`0-1`) | 4 GB | 2048 / 1638 / 410 MB |
  | M | 4 (`0-3`) | 8 GB | 4096 / 3277 / 819 MB |
  | L | 8 (`0-7`) | 16 GB | 8192 / 6554 / 1638 MB |

- **PostgreSQL 16.15** is not part of the budget: its own cpuset `8-31`, no memory limit, one fixed
  configuration in every tier (`shared_buffers=4GB`, `effective_cache_size=16GB`,
  `maintenance_work_mem=1GB`, `max_wal_size=8GB`, `checkpoint_timeout=15min`; everything else stock, so
  `max_connections=100`, `synchronous_commit=on`, `wal_level=replica`, `fsync=on`). The generator shares the
  host with Docker Desktop's VM and may compete for cores with PostgreSQL's range.
- Non-default zirv-flow settings: `ingest.drain_shards` stays at the shipped 8; the per-key ingestion limit
  is raised from 10,000/s (burst 20,000) to 20,000/s (burst 40,000) for the burst run and to 40,000/s
  (burst 80,000) for the batch ramps; `log.filter=warn`; the session cookie is not `Secure`. Everything
  else is the default.
- Every test recreates the stack on a fresh volume, so no test sees earlier data (stricter than once per
  tier). `docker stats` and the inbox (rows and oldest-row age) are sampled every few seconds into the data
  directory; per-container peak CPU is a percentage of one core.

### 2.2 Tests

| Test | What |
|---|---|
| `track1`, `identify1` | ceiling ramp, one workspace: from 250 items/s in 250 steps, 90 s per step |
| `track4` | the same ramp for track across 4 workspaces, 4 generators with one key each at rate/4 |
| `batch1` | ramp in batches of 100 (identify and track mixed), 2,000 items/s per step, to expose app limits the single-item host path hides |
| `sustained` | track at 2,000/s for 10 min on one workspace |
| `burst` | batches of 100 at 10,000/s for 2 min, then the time for the inbox to empty (cap 30 min) |
| `batch` (M only) | 500 per request at 2,000 items/s for 10 min |
| `decappend` (M only), A/B | section 6 |
| `oq1` (M only) | section 7 |

A ramp step **holds** only if all of these are true: the schedule-based API p99 is under 100 ms; there are no
5xx, 429 or transport errors; the inbox's oldest row is under 5 s old at the end of the step and did not grow
past its mid-step age by more than 3 s; the generator kept its schedule (`late_dispatch` 0 and at least 98%
of the target achieved). The **ceiling** is the last step that held. A step with `late_dispatch` above 0 is
generator-bound and is not evidence about the server.

**Latency.** The API figure is `latency_from_schedule`: from each request's scheduled time to its final
response, so a stalled server and a generator that fell behind both show. `service_latency` (send to
response) and `dispatch_lag` are reported beside it in the data. The inbox-to-applied figure is
`scripts/bench/applied-latency.sql`: the decision's `recorded_at` minus the inbox arrival, scoped to the step
or run by its start time. It is the start of the draining transaction, so it slightly under-reads; and for
identify it is one value per source record (its latest change).

### 2.3 Environment

| | |
|---|---|
| CPU | 13th Gen Intel Core i9-13900K, 24 cores / 32 threads |
| RAM | 63.8 GB |
| Disk | NVMe SSDs (WD_BLACK SN850X 2 TB, Samsung 990 PRO 1 TB) |
| OS | Windows 11 Pro 10.0.26300 |
| Docker | Docker Desktop 4.63.0, engine 29.2.1, 32 CPUs and 31.2 GiB visible to containers (no limits set) |
| PostgreSQL | 16.15, settings above |
| zirv-flow | release image, musl static, `lto=true`, image `sha256:d6903f9873ed...` for the final runs |
| Generator | `loadgen` 0.30.0, release build, on the host |

The host was quiet during every run (no build or test processes); the runs were started from a script that
waits while `cargo`, `rustc` or `nextest` run.

### 2.4 Round-trip context

A client-side hostname turned out to matter more than any tier setting, so it is stated first:

| Path | `select 1` round trip |
|---|---|
| container to PostgreSQL, PostgreSQL pinned to cores 8-31 | p50 35 us, p95 59 us, p99 93 to 100 us (10,000 queries, two runs) |
| container to PostgreSQL, unpinned | p50 36 us, p95 58 us, p99 84 us |
| host to PostgreSQL through the published port, name `localhost` | about 4.4 ms per statement |
| host to PostgreSQL through the published port, `127.0.0.1` | about 0.33 ms per statement |

Pinning PostgreSQL did not change the round trip, so it stayed pinned for every run. Aiming the generator at
`localhost` instead of `127.0.0.1` raised the API p99 at 2,000/s from 9.2 ms to 150.7 ms in a 45-second
step (new connections are slow through that name); all runs below use `127.0.0.1`. The **baseline** runs
(section 4) used `localhost`, so their short-step API p99 figures are pessimistic; their drain and
applied-latency figures are not affected by it. The decision A/B (section 6) runs the worker on the host
against the forwarded port and therefore pays the 0.33 ms path on every statement.

## 3. Capacity ceilings (single workspace, track)

Ceiling in items/s, last held step; "fails at" is the first failing rung and its reason.

| Tier | vCPU / RAM | Before (0.28 drain, 500 steps) | After: track | identify | track, 4 workspaces | batch ramp (steps of 2,000) |
|---|---|---|---|---|---|---|
| S | 2 / 4 GB | 500 | 2,000 (fails at 2,250: API p99 117.8 ms) | 2,000 (2,250: API p99 137.2 ms) | 1,750 (2,000: API p99 117.5 ms) | 2,000 (4,000: inbox age 22.7 s) |
| M | 4 / 8 GB | 500 | 2,750 (3,000: API p99 198.2 ms) | 2,500 (2,750: API p99 109.9 ms) | 2,250 (2,500: API p99 128.8 ms) | 2,000 (4,000: inbox age 18.5 s) |
| L | 8 / 16 GB | 500 | 3,000 (3,250: generator late dispatch, see below) | 2,750 (3,000: API p99 and inbox age growing) | 2,750 (3,000: achieved 2,460, transport errors) | 2,000 (4,000: inbox age 18.9 s) |

Before, for track across 4 workspaces: 1,500 (S), 2,000 (M), 2,500 (L); identify one workspace: 500 in
all tiers. Before, every single-workspace ramp failed at 1,000 on the drain (inbox age above 10 s and growing).
After, the single-item ramps no longer fail on the drain.

Peaks at the ceiling step (percent of one core / MiB not shown):

| Tier, test | serve | worker | PostgreSQL (cores 8-31) |
|---|---|---|---|
| S track (2,000) | 91 | 73 | 320 |
| M track (2,750) | 206 | 85 | 431 |
| L track (3,000) | 339 | 91 | 499 |
| S track4 (1,750) | 80 | 102 | 392 |
| M track4 (2,250) | 149 | 162 | 548 |
| L track4 (2,750) | 302 | 233 | 710 |
| S, M, L batch (2,000) | 4 to 5 | 70, 90, 97 | 539, 340, 329 |

### 3.1 What saturates

- **Tier S is limited by the CPU budget.** At 2,000 items/s serve and worker use 164% of the 200% available;
  the next rung fails on API p99.
- **Tiers M and L are not limited by their CPU budget in these tests.** The app uses 73% (M) and 54% (L) of
  its cores at the ceiling. API p99 is what fails, with serve the busiest container. The host-to-container
  path caps out near 3,000 single-item requests per second: at L's next rung the generator fell behind by
  182,107 late dispatches with service p99 of 4.2 s, and at L's 3,000/s four-workspace step 37,558
  requests failed as transport errors. L's track ceilings (3,000 and 2,750) are therefore lower bounds set by
  the measuring path, not by the server.
- **One workspace's drain was bound near one core (before #74).** The worker peaked at 100% in the batch
  ramps (4,000 items/s step), 103 to 105% in the 10,000/s burst of every tier and 73 to 97% in the
  single-workspace ceilings, regardless of tier. The burst drain rate (about 3,400 items/s of mixed
  identify and track) was the same in S, M and L (section 5). With four workspaces the worker reached 162%
  (M) and 233% (L), so the limit was per workspace: one workspace's drain joined its shard loops in one
  task, which runs on one thread at a time (#74).
  **After #74** (each shard its own task, section 5.1) the same burst takes the worker to 177% (S, of 200%
  available), 238% (M) and 274% (L), and the drain rate rises to about 4,400 (S), 4,000 (M) and 4,000 (L)
  items/s: the per-workspace cap of one core is gone, but the rate gained is about 18 to 31%, not the
  2 to 4 times the extra cores might suggest, and it no longer follows the vCPU count (M and L are equal;
  L's worker uses 0.36 core more for the same rate). PostgreSQL peaks at 450 to 490% in these bursts (325
  to 345% before), so the next limit was not located by these runs.
- PostgreSQL peaks at 300 to 700% of the cores it has (24), it is not the limit.

### 3.2 Scaling and how to predict

| Doubling | track1 | identify1 | track4 | batch ramp |
|---|---|---|---|---|
| S to M | x1.38 | x1.25 | x1.29 | not resolved |
| M to L | >= x1.09 | >= x1.10 | >= x1.22 | not resolved |

The batch ramp has no scaling factor. Its steps are 2,000 items/s (first step 2,000, then 4,000), 2,000 held
and 4,000 failed in every tier, so each tier's ceiling lies between 2,000 and 4,000 items/s and the ramp
cannot show whether the three tiers have equal or different batch capacity. The 2,000 is the first step,
not a measured limit.

Predicting a ceiling for another setup:

- Single-item ingestion costs about 0.8 core per 1,000 items/s at the S ceiling (serve about 0.45, worker
  about 0.37). With 2 vCPU that predicts 2.4 k/s; the measurement is 2.0 k/s held and 2.25 k/s failing, so
  treat the model as accurate to about 20% for small budgets.
- Beyond about 4 vCPU the ceiling stops following the CPU budget. Use the smaller of three limits: the CPU
  model above, about 3,000 single-item requests per second from this host path (use batch requests above
  that), and a drain of about 3,400 mixed items per second per workspace for sustained load before #74
  (one workspace's drain saturated one core, so more vCPU did not raise it). After #74 the measured drain
  is about 4,000 to 4,400 mixed items per second per workspace, the same in M and L, so it is not
  predicted by the vCPU count either; use 4,000 until a run locates the next limit.
- Do not extrapolate beyond about twice the largest tier (16 vCPU): there are only three points, two of them
  are bounded by the measuring path rather than the server, and the drain limit is a property of the code at
  this commit.

## 4. The drain fix, before and after

Tier M, sustained track at 2,000/s for 10 minutes on one workspace (1.2 million items), before (0.28 drain,
one drain loop per workspace) and after (PR #73):

| | Before | After, 4 shards (clean run) | After, 8 shards (clean run) | Final run (defaults, 8 shards) |
|---|---|---|---|---|
| Applied p50 | 401.7 s | 6.9 s | 11.5 ms | 11.9 ms |
| Applied p95 | 642.8 s | 16.9 s | 31.0 ms | 38.1 ms |
| Applied p99 | 664.5 s | 19.2 s | 125.7 ms | 448.1 ms |
| Applied max | 669.9 s | 20.6 s | 2.8 s | 1.7 s |
| Drain after the generator stopped | 672 s | 13 s | 3 s | 0 s |
| Largest backlog | 705,862 rows | 29,354 | 647 | 4,489 rows (oldest 3.2 s) |
| Deadlocks in the PostgreSQL log | 0 | 1 | 1 | 0 |
| Peak worker connections | n/a | 14 | 16 | 17 |

The default moved from 4 shards to 8 because of this: with 4, one workspace sustained 2,000/s but with a
standing backlog (applied p95 16.9 s, target 5 s); with 8 it keeps up. The 4-against-8 runs are in
`data/shards-4/M` and `data/shards-8/M`; the baseline in `data/baseline`. Those two used the `localhost`
generator target; the drain results do not depend on it.

## 5. Sustained, burst and batch results

Sustained, track at 2,000/s for 10 minutes, one workspace, after (before in brackets):

| Tier | Applied p50 / p95 / p99 / max | Drain after stop | API p99 | Peak CPU serve / worker / PostgreSQL |
|---|---|---|---|---|
| S | 11.6 / 45.8 / 485.2 / 2,807 ms (403 / 649 / 669 / 674 s) | 3 s (673 s) | 80.2 ms (13.5 ms) | 96 / 79 / 683 (131 / 32 / 194) |
| M | 11.9 / 38.1 / 448.1 / 1,674 ms (402 / 643 / 665 / 670 s) | 0 s (672 s) | 11.0 ms (9.7 ms) | 166 / 89 / 702 (187 / 32 / 201) |
| L | 12.1 / 35.1 / 119.4 / 2,145 ms (396 / 618 / 638 / 643 s) | 3 s (643 s) | 12.7 ms (10.0 ms) | 243 / 84 / 693 (177 / 33 / 215) |

All six generators were valid (sustained: 1,200,000 items accepted in S, 1,199,999 in M and 1,199,998 in L; burst: 1,200,000 in each tier; every accepted request answered 202, `late_dispatch` 0). Deadlocks in the
PostgreSQL log: 0 in every tier's sustained run. S's API p99 of 80.2 ms is close to the limit: tier S has
no spare CPU at 2,000/s (section 3.1).

Burst, 10,000 items/s for 2 minutes (1.2 million items in batches of 100, mixed identify and track), then
the time for the inbox to empty; before in brackets:

| Tier | Largest backlog | Drain after the generator stopped | Applied p95, per kind (the burst mixes identify and track) | API p99 | Worker peak CPU | Deadlocks |
|---|---|---|---|---|---|---|
| S | 814,411 rows | 243 s (1,241 s) | identify 225.7 s, track 223.6 s (1,190.5 s, 1,173.8 s) | 23.2 ms | 105% | 1 |
| M | 796,475 | 237 s (1,285 s) | identify 223.2 s, track 220.7 s (1,232.0 s, 1,215.8 s) | 22.4 ms | 103% | 9 |
| L | 806,822 | 240 s (1,283 s) | identify 223.0 s, track 220.9 s (1,231.4 s, 1,214.8 s) | 22.6 ms | 104% | 1 |

The drain keeps pace for about 3,300 items/s during the burst and empties the backlog at about 3,400 items/s,
the same in every tier. The 10-minute burst of the target would offer 6 million items; at that drain rate the
backlog after 10 minutes is about 4 million and takes about 19 to 20 minutes to empty, above the 15-minute
target. That is a projection, not a measurement (the burst was run for 2 minutes, per the plan); the number
has the mixed identify and track workload behind it, and track alone may drain faster
(issue: #74).

Batch endpoint (tier M), 500 items per request at 2,000 items/s for 10 minutes (before in brackets): 1.2 million
items accepted, API p99 124.9 ms (32.5 ms) across 4 requests per second, applied p50 / p95 / p99 / max
253 ms / 359 ms / 3.7 s / 7.8 s (352 / 710 / 743 / 751 s), drain 3 s (752 s). The API p99 target is missed
for 500-item requests; a batch is a larger body to parse before it is appended
(issue: #76).

### 5.1 After #74: each drain shard on its own task

Issue #74: one workspace's `ingest.drain` joined its 8 shard loops in one task and so used one core. The
branch `fix/drain-cores` (version 0.31.0, on `main` at eb79a11, tooling commit 201a699) spawns each shard as its
own tokio task. Measured with the same harness, tiers and settings as above, on a fresh stack per run, on a
quiet host (no `cargo`, `rustc` or `nextest` process from 17:48 local time until the last run ended at
18:45), release image `sha256:58e3c73f347c...`, generator on `127.0.0.1`. Everything before this subsection is
unchanged. Raw outputs: `data/after-74/{S,M,L}` and `data/after-74/burst600/M`.

Burst, 10,000 items/s for 2 minutes (batches of 100, mixed identify and track); the first value is after
#74, the before value from the table above in brackets. The drain rate is the backlog at the generator's stop
divided by the drain time:

| Tier | Largest backlog | Drain after the generator stopped | Drain rate | Applied p95, per kind | API p99 | Worker / PostgreSQL peak CPU | Deadlocks |
|---|---|---|---|---|---|---|---|
| S | 703,783 rows (814,411) | 160 s (243 s) | 4,400/s (3,350/s) | identify 149.8 s, track 147.3 s (225.7 s, 223.6 s) | 24.6 ms (23.2 ms) | 177% / 454% (105% / 345%) | 4 (1) |
| M | 743,392 (796,475) | 186 s (237 s) | 4,000/s (3,360/s) | identify 174.8 s, track 172.1 s (223.2 s, 220.7 s) | 23.5 ms (22.4 ms) | 238% / 488% (103% / 340%) | 5 (9) |
| L | 737,813 (806,822) | 186 s (240 s) | 3,970/s (3,360/s) | identify 171.5 s, track 169.2 s (223.0 s, 220.9 s) | 25.5 ms (22.6 ms) | 274% / 469% (104% / 325%) | 6 (1) |

Largest backlog is the larger of the inbox samples and the count at the generator's stop. The drain is 22 to
34% shorter, not the 2 to 4 times the extra cores would allow; M and L drain at the same rate (section 3.1).
Applied p95 falls with the shorter drain but stays above 140 s because a burst of this size is a standing
backlog. All three runs were valid: 1,199,900 to 1,200,000 items accepted (all 202), achieved at least 99.97%
of the target, `late_dispatch` 0, no 5xx, 429 or transport errors, and no container crashed.
The deadlocks were retried by the drain (as in section 8).

Sustained regression check, tier M, track 2,000/s for 10 minutes (before in brackets, section 5): applied p50
/ p95 / p99 / max 11.4 / 39.8 / 62.1 / 2,397 ms (11.9 / 38.1 / 448.1 / 1,674 ms), drain after stop 3 s (0 s),
largest backlog 792 rows (4,489), API p99 21.9 ms (11.0 ms), peak CPU serve / worker / PostgreSQL 156 / 112 /
666% (166 / 89 / 702%), 0 deadlocks. 1,200,000 items accepted, `late_dispatch` 0, no errors. Applied p95 is
within 5% of the earlier run and every target still holds. API p99 is twice the earlier figure (still a
tenth of the 100 ms limit); this is one run and the earlier figure is one run, so whether the difference
is the change or run-to-run variation is not known from these two measurements.

The section 19 case, tier M, burst 10,000 items/s for 10 minutes (6 million items in batches of 100),
then the drain to empty:

| Largest backlog | Drain after the generator stopped | Drain rate | Applied p95, per kind | API p99 | Worker / PostgreSQL peak CPU | Deadlocks | Result |
|---|---|---|---|---|---|---|---|
| 3,640,933 rows (oldest 369 s) | 890 s (14.8 min) | 4,090/s | identify 867.7 s, track 839.2 s | 23.9 ms | 245% / 542% | 7 | HIT, 10 s inside 15 min |

All 6,000,000 items were accepted (60,000 requests, all 202, achieved 9,999.7/s, `late_dispatch` 0, no
errors); 3,001,301 track events and 997,535 people were stored and the inbox ended at 0. The worker averaged
224% over the drain; from about 350 s in, single samples dip as low as 72%, and in the last 5 s, as the
inbox empties, it falls to 0. The margin is 1.1% of the target: the result is a pass on this host, one
run, and 4,090 items/s is the measured rate for this workload (mixed identify and track, 1,000,000 people), so
a run a little slower would miss. It is a development machine, not reference hardware.

## 6. Decision-log overhead

Section 19: under 5 ms added transaction time (p95) and under 10% throughput loss against the same workload
with decision writes disabled. There is no decision-append duration metric in the code, so two direct
measurements were made.

**Added transaction time.** One drain batch writes one decision per applied item (plus a `person.created`
for a new person), so a batch of 500 track events appends 500 decisions on the drain transaction. The store
test `a_drain_batch_of_decisions_records_its_duration` times that, 500 batches per shape, one
`append_decision` per decision as the drain does:

| Path | Shape | Decisions per batch | p50 | p95 | p99 |
|---|---|---|---|---|---|
| host to PostgreSQL (`127.0.0.1`, 0.33 ms per statement) | track | 500 | 166.3 ms | 184.5 ms | 233.7 ms |
| | identify | 500 | 168.4 ms | 184.8 ms | 236.1 ms |
| | new person plus event | 1,000 | 335.5 ms | 387.9 ms | 418.2 ms |
| container network (`pgbench`, one 500-insert transaction, 600 batches) | track | 500 | 44.0 ms | 52.9 ms | 63.6 ms |

The container-network row is the deployment-shaped figure: about 0.1 ms per decision, 52.9 ms of added p95
per 500-item drain transaction. The drain transaction is the unit that commits the decisions, so against
"under 5 ms of added transaction time" that is a miss by about 10x (issue: #75); read per
event it is 0.1 ms. The statements are issued one by one; the code does have a batched
`append_decisions`, which the drain does not use.

**Throughput loss.** Native release builds, unconstrained (not in the tier budgets), the same PostgreSQL;
a backlog of 185k to 193k track events is preloaded with no worker running, then the worker and scheduler
are started and the time to empty is measured. `off` is built with `bench-no-decisions`:

| Run | Backlog | Drain | Applied items/s | Decision records |
|---|---|---|---|---|
| on, 1 | 184,696 | 175.4 s | 1,053 | 268,875 |
| off, 1 | 193,490 | 119.4 s | 1,621 | 0 |
| on, 2 | 190,473 | 176.8 s | 1,077 | 275,476 |
| off, 2 | 191,551 | 117.9 s | 1,625 | 0 |

Mean 1,065 items/s with decisions against 1,623 without: a **34.4% loss**, a miss against 10%
(issue: #75). Caveats: the backlogs are about 190k rather than the intended 600k
because the generator, not the server, limited the preload on this host path (it is a drain
comparison, and the sizes of the sides differ by under 5%); the native worker pays 0.33 ms per statement
to the forwarded port instead of 35 us in the container network, which inflates the cost of the extra
statements, so the loss in a containerised deployment is lower than this; that was not measured. The
order was on, off, on, off.

## 7. OQ-1: trait storage

1,000,000 people, 20 traits of mixed type (six strings, five numbers, four booleans, three timestamps or
dates, two lists), on tier M's PostgreSQL in its own schemas, 5 runs per statement, medians, warm cache,
each statement under `EXPLAIN (ANALYZE, BUFFERS)`:

- **A:** `traits jsonb` with expression indexes on three hot traits (`country` text equality,
  `ltv` numeric range, `signup_at` timestamp range; the timestamp index needs an `IMMUTABLE` wrapper
  `oq1_ts(text)` because `text::timestamptz` is only `STABLE`).
- **B:** those three promoted to typed columns (`text`, `numeric`, `timestamptz`) with btree indexes, the other 17
  traits in `jsonb`.

| Statement (rows matched) | A exec (ms) | B exec (ms) | B vs A | A buffers | B buffers |
|---|---|---|---|---|---|
| equality count (about 20,000) | 27.4 | 1.3 | 21x faster | 17,753 | 22 |
| equality ids | 27.8 | 31.2 | 12% slower | 17,753 | 17,446 |
| range count (about 10,000) | 18.1 | 1.0 | 17x faster | 9,243 | 2,008 |
| range ids | 18.1 | 19.1 | 6% slower | 9,243 | 9,165 |
| 3-trait AND count (2,710) | 46.9 | 26.3 | 1.8x faster | 12,212 | 6,030 |
| 3-trait AND ids | 46.0 | 26.1 | 1.8x faster | 12,212 | 6,030 |
| upsert of 10,000 trait changes | 163.8 | 132.9 | 1.23x faster | 189,757 | 189,525 |

Table sizes: A 546.5 MB heap, B 482.1 MB (12% smaller); indexes 74.5 MB in both. Wall times are within 1
ms of exec times. Plans are in `data/M/oq1-plans`. Planning time is 0.3 ms throughout.

**Decision: promote hot traits to typed columns (B); keep everything else in `jsonb`.** The difference is
not within noise for the statements a segment engine runs most: counts are 17 to 21 times faster because typed
columns allow index-only scans, which an expression index on `jsonb` cannot, and the three-trait AND is 1.8
times faster. The id scans, which must visit the heap either way, are within 6 to 12% of each other and
are the only place A is ahead. The upsert is 23% faster in B. Costs of B to carry into the design: a column
is a migration and a promotion process (which traits are hot is a workspace-level decision), the write path
must keep a column and the `jsonb` copy of a trait consistent while one trait is moving, and a
promoted trait's type must be fixed. Measured at 1 million rows with a warm cache; at 10 million rows the
gap in buffer reads, which is what grows, would widen rather than shrink, but that is not measured here.
Recorded in architecture 21.1.

## 8. Invalid or repeated runs

- A first set of runs (identify at 2,000/s on an unconstrained stack) was stopped and discarded when the
  plan changed to resource tiers.
- The drain fix was measured twice before the final runs: a first pass was contaminated by builds running on
  the host and was discarded; the clean 4-shard and 8-shard runs are kept (section 4).
- The L-tier identify ramp was run twice. In the first, its lowest rung (250 items/s) recorded one transport
  error among 22,500 requests and the strict no-errors rule failed the step, giving a ceiling of 0 that says
  nothing about capacity. It was re-run once, unchanged, and the valid result is in section 3; the first
  attempt is kept in `data/L/invalid-first-attempt`.
- L tier track at 3,250 items/s and four-workspace track at 3,000 items/s are generator or host-path bound
  (section 3.1), so L's track ceilings are lower bounds.
- An earlier attempt at the decision timing and at OQ-1 produced nothing: the first was slow because it
  connected through the name `localhost` (section 2.4), the second ran out of shared memory in the container
  and the runner did not fail. Both are fixed in `scripts/bench/tier.sh` (it now uses `127.0.0.1`, sets a larger
  `/dev/shm` for OQ-1, and exits non-zero on any failing step); the numbers above are from the fixed runs.
- Deadlocks: the PostgreSQL log showed one in the first seconds of each 4- and 8-shard sustained run, none in the final
  sustained runs of any tier, and 1, 9 and 1 in the S, M and L bursts. The drain retries them; there were no 5xx
  and no lost items (events stored equal items accepted in every run).

## 9. Data

Raw outputs are in `docs/benchmarks/data/`, one directory per tier (`S`, `M`, `L`), per test: the generator
report (`<test>.json`), the ramp steps (`<test>-steps.jsonl`), the applied-latency CSV, the inbox and
`docker stats` samples, PostgreSQL facts (deadlocks, connections), the settings (`<test>-stack.txt`). Also
`baseline/` (before), `shards-4/` and `shards-8/` (section 4), `ab/` (section 6) and
`M/decision-append*.json`, `M/oq1.json`. Section 5.1 (after #74): `after-74/{S,M,L}` (2-minute burst in
each tier, tier M also the sustained regression check) and `after-74/burst600/M` (the 10-minute burst). They contain no keys or secrets. The harness is
`scripts/bench/` (runbook: `scripts/bench/README.md`).
