# Tier L flow rerun on 1.0.39: the stale-statistics and overload fixes

1.0.39 (PR #874) changed two things after the [tier L report](tier-l-capacity.md) on 1.0.38:

- **Stale statistics.** Migration 0116 rebuilds `enrollments_in_flight_idx` person first and adds
  `enrollments_held_idx`. The re-entry check and the goal/exit lookups then probe once per person,
  even when the statistics say nothing is in flight (that report, section 6, item 9).
- **Pool wait.** Serve waits at most 5 s for a database connection, and an ingestion request that
  cannot get one is answered 503 with `Retry-After: 5`. Before, the wait was sqlx's default of 30 s
  (item 10).

This rerun measures both. It used the same stack, flow and ramps as passes 1 to 3 of the 1.0.38
report. Raw data is in [`data/tier-l-1.0.39/`](data/tier-l-1.0.39/).

**This is a development machine, not reference hardware.** The host and stack are the same as in the
1.0.38 report: an i9-13900K, Windows 11 and Docker Desktop. The app has cpuset 0-7. PostgreSQL 16.16
has cpuset 8-15 and 16 GB. The serve pool is 32. The load generator runs on the host.

## 1. Summary

- **The plan flip is fixed.** Every step of the forced-stale arm started with the statistics showing
  no enrollment in flight. Under that condition 1.0.38 got slower step after step and failed at 800
  triggers/s: trigger-to-enrollment p95 88 s, ingest applied p95 86 s. Its slow statements were the
  re-entry check (271) and the goal/exit lookup (188). 1.0.39 recovered by the next step (p95 1.06 s
  at 600). Neither statement was slow in any 1.0.39 run (section 3.1).
- **The transient stalls are not fixed.** Single steps still stall for about a minute at the same
  places as on 1.0.38:
  - concurrency 4 `ramp-0` step 250: p95 9.4 s;
  - concurrency 16 step 400: p95 12.6 and 13.7 s;
  - concurrency 32 step 400: p95 6.1 and 5.4 s.

  Below saturation, at 200 triggers/s and up, a step stalled when the first autovacuum pass inside
  it came 45 s or more after the step began: 5 of 5 such steps stalled. Of the other 29 such steps,
  only the one where overload began went above 3 s. Every stall drained 9 to 36 s after that pass,
  which analyzes `jobs`, `flow_entries` and `enrollments`. So the stalls are still a
  stale-statistics effect from the idle gap between steps, but in some query other than the ones 0116
  fixed. Which query is not established (section 3.2).
- **Overload is now refused, but late, and the 30 s timeouts come from the load-generator host.** In
  the two steps that overloaded, serve answered 503 (29,631 and 29,858 refusals) where 1.0.38 answered
  none. No pool wait exceeded 5.0 s. A refusal still takes about 9 to 10 s, because each ingest request
  takes two connections one after the other, and each may wait 5 s. The 30 s transport errors line up
  with Windows Tcpip event 4227 in both 1.0.39 overload steps and in the 1.0.38 collapse. One step also
  has 4231, "all such ports being in use". At about 9.5 s per request the two generators hold more connections than
  the host's 16,384 dynamic ports (section 4).
- **Capacity is unchanged within run-to-run variation.** At equal steps, enrollments/s and PostgreSQL
  CPU match 1.0.38 (section 5). The onset of overload varied between runs. At concurrency 16, run 1
  failed at 900 and run 2 held to the maximum of 1,000 (660 enrollments/s). At concurrency 32, run 1
  failed at 800 and run 2 held to 1,000.
- **PostgreSQL failed twice more in the benchmark's own metrics query.** A signal 11 ended the
  forced-stale 1.0.39 arm at step 700. "pfree called with invalid pointer" ended concurrency 32 run 2
  at step 1,100. That makes three failures, all in `scripts/bench/flows/metrics.sql` and none in a
  product request (section 6).

## 2. What ran

All runs used image 1.0.39 (`sha256:8329ab73…`, created 2026-11-26T11:02:14Z), except the control,
which used 1.0.38 (`sha256:55ad1409…`).
- Every run started from a fresh database: `flows.sh up` removes the volumes, then `seed 100000`
  (94,980 people) and `setup`.
- Every run had the 60 s `warmup` window at 50 triggers/s. Runs without forced statistics also had the
  300 s `bg1000` window.
- Ramps use 120 s steps and the `flows.sh` criteria.
- Every run had diagnostic logging on: `log_autovacuum_min_duration` 0, `log_lock_waits` on,
  `log_min_duration_statement` 2 s (`diag-logging.txt`). `autovacuum_naptime` was the default of 1 min.
- The runs ran one after another on 2026-11-26/27 (times in UTC).

| Run | Image | Concurrency | Ramps (background: start, step, max) | Result |
|---|---|---|---|---|
| `flows-conc16` | 1.0.39 | 16 | 1,000: 100, 100, 1,000 | held 800; failed at 900 (generators below 98%) |
| `flows-conc16-stale` | 1.0.39 | 16 | 1,000: 100, 100, 800; `ANALYZE enrollments` before every step | held 600; step 700 lost to a PostgreSQL crash |
| `control-1.0.38-conc16-stale` | 1.0.38 | 16 | the same | held 700; failed at 800 (p95 over 60 s) |
| `flows-conc32` | 1.0.39 | 32 | 1,000: 100, 100, 1,200 | held 700; failed at 800 (generators below 98%) |
| `flows-conc4` | 1.0.39 | 4 | 0: 100, 50, 450; 1,000: 100, 50, 500; 2,000: 100, 50, 400 | held 450 (max), 350 and 300 |
| `flows-conc16-repeat` | 1.0.39 | 16 | as `flows-conc16` | held to 1,000 (max) |
| `flows-conc32-repeat` | 1.0.39 | 32 | as `flows-conc32` | held 1,000; step 1,100 lost to a PostgreSQL error |

The two ramps cut short have a `ramp-1000.json` assembled from their per-step records, and its
`verdict` says so. Each step file has a new key, `enrollments_state_statistics`. It records the
in-flight share that the planner's statistics hold for `enrollments.state`, plus the analyze counters,
at the step's start and end (`scripts/bench/README.md`). With `ANALYZE enrollments` before every step,
the share was 0 at the start of every step in both forced-stale arms. In the other runs it was also 0
at the start of 11 of 58 steps, from the autoanalyze in the idle gap.

## 3. Stale statistics

### 3.1 Forced-stale arm: 1.0.38 against 1.0.39

Concurrency 16, background 1,000 events/s. Before every step `ANALYZE enrollments` ran with nothing
in flight. Files: `control-1.0.38-conc16-stale/flows/` and `flows-conc16-stale/flows/`.

| Triggers/s | 1.0.38 enrollments/s | 1.0.38 trig-to-enroll p50 / p95 (s) | 1.0.38 applied p95 (ms) | 1.0.39 enrollments/s | 1.0.39 trig-to-enroll p50 / p95 (s) | 1.0.39 applied p95 (ms) |
|---|---|---|---|---|---|---|
| 100 | 97.5 | 0.017 / 0.056 | 29.9 | 97.5 | 0.018 / 0.060 | 31.7 |
| 200 | 189.9 | 0.020 / 0.035 | 21.1 | 189.9 | 0.019 / 0.035 | 20.9 |
| 300 | 277.8 | 0.028 / 0.055 | 30.7 | 277.8 | 0.026 / 0.050 | 28.4 |
| 400 | 361.7 | 0.040 / 0.140 | 51.1 | 361.8 | 0.034 / 0.078 | 37.4 |
| 500 | 416.0 | 3.84 / 20.4 | 10,266.8 | 428.8 | 2.69 / 14.2 | 420.3 |
| 600 | 439.4 | 10.2 / 35.0 | 24,349.5 | 516.8 | 0.713 / 1.06 | 43.0 |
| 700 | 482.1 | 13.5 / 42.8 | 35,310.5 | no result (crash) | | |
| 800 | 192.1 (failed) | 58.4 / 87.9 | 86,281.1 | not run | | |

Both arms stalled at step 500. 1.0.38 then got worse with each step and failed. 1.0.39 was clean
again at 600, at the same enrollments/s as the runs without forced statistics (516 to 518).

The worker's slow statements (over 1 s) show what changed (`worker-slow-statements.txt`):

| Run | Slow statements | Re-entry check | Goal/exit lookup | `people` row lock | `jobs` |
|---|---|---|---|---|---|
| control 1.0.38, forced stale | 501 | 271 | 188 | 41 | 0 |
| 1.0.39, forced stale | 14 | 0 | 0 | 14 | 0 |
| `flows-conc16` | 343 | 0 | 0 | 18 | 325 |
| `flows-conc16-repeat` | 8 | 0 | 0 | 8 | 0 |
| `flows-conc32` | 454 | 0 | 0 | 46 | 408 |
| `flows-conc32-repeat` | 218 | 0 | 0 | 192 | 26 |
| `flows-conc4` | 0 | 0 | 0 | 0 | 0 |

The re-entry check is `SELECT person_id FROM enrollments WHERE flow_id = $1 AND person_id = ANY…`.
The goal/exit lookup is `SELECT e.id, e.person_id, t.event_id, …`. Neither was slow in any 1.0.39
run. The `jobs` statements in `flows-conc16` and `flows-conc32` fall in their overload steps (section 4),
all but one, which is in concurrency 32 step 400.

### 3.2 The transient stalls that remain

| Run | Step | trig-to-enroll p50 / p95 (s) | Oldest due (s) | p95 one step below / above (s) | First autovacuum pass in the step (s after start) |
|---|---|---|---|---|---|
| `flows-conc4` `ramp-0` | 250 | 0.152 / 9.44 | 11.1 | 0.024 / 0.157 | 56 |
| `flows-conc16` | 400 | 0.701 / 12.6 | 11.9 | 0.047 / 0.137 | 51 |
| `flows-conc16-repeat` | 400 | 0.927 / 13.7 | 13.9 | 0.058 / 0.227 | 50 |
| `flows-conc32` | 400 | 0.060 / 6.09 | 4.0 | 0.051 / 0.091 | 50 |
| `flows-conc32-repeat` | 400 | 0.059 / 5.38 | 5.3 | 0.055 / 0.091 | 49 |
| `flows-conc16-stale` | 500 | 2.69 / 14.2 | 14.6 | 0.078 / 1.06 | 49 |

The pass time is that of the first `automatic analyze of table "…jobs"` after the step's `since`
(`autovacuum.txt`, which lists every autovacuum and autoanalyze of a `public` table). The same pass
analyzes `flow_entries` and `enrollments` within 9 s. Concurrency 4 `ramp-0` step 250 and
concurrency 16 step 400 stalled on 1.0.38 too. Concurrency 32 stalled at 700 on 1.0.38 and at 400 here.

Concurrency 4 step 200 at 1,000 and at 2,000 events/s also had a late first pass (51 and 53 s) and a
p95 near 9 s. They are not in the table, because they are the onset of saturation, not a stall the
pass ended. Enrollments/s were 183.7 and 174.7 against 200, every later step was slower, and the queue
drained 72 and 77 s after the pass.

**The timing follows the autovacuum cycle.** Autovacuum visits the database once a minute. One step
with its drain and the gap lasts about 165 s, so the offset of the first pass inside a step moves back
about 15 s per step: 10, 25, 35, 50, then 10 again. Concurrency 16 run 1 shows it: 11, 26, 36, 51
(stall), 10, 24, 36, 40, 45 s.

Below saturation there were 34 steps with p50 under 1 s at 200 triggers/s or more: 31 on 1.0.39 and
3 from the 1.0.38 control's early steps. Of these, 5 had their first pass 45 s or more after the
start, and all 5 stalled (p95 5.4 to 13.7 s). Of the other 29, one went
above 3 s: concurrency 32 run 1 step 800 (3.2 s), the step where overload began.

In the samples (`*-samples.ndjson`), each of the six stalls builds from about 15 to 35 s into the
step. The oldest due job's age peaks around the first pass, up to 12 s before it, and the queue drains
9 to 36 s after the pass. For example, concurrency 16 run 1, step 400 started at 20:07:10:
- the oldest due job reached 11.9 s at 20:07:58;
- `jobs` was vacuumed and analyzed at 20:08:01;
- `enrollments` was analyzed at 20:08:04;
- the due queue peaked at 6,466 jobs at 20:08:05;
- the queue was at 3 jobs by 20:08:22.

**During a stall no single statement is slow.**
- The worker's slow statements are the `people` row lock:
  `WITH locked AS MATERIALIZED (SELECT … FROM people WHERE id = ANY($2) ORDER BY id FOR UPDATE)`,
  1.0 to 3.4 s.
- In `flows-conc32-repeat` the job claim (`UPDATE jobs SET state = 'running' …`) was slow 7 times.
- PostgreSQL logged lock waits on `people` tuples (`postgres-lock-waits.txt`; 1.0 to 4.3 s across
  the runs).
- The concurrency 4 stall has no slow statement at all.

So many short statements each got slower. A statement that turned slow on its own would have shown in
the log.

**What follows from this.**
- Shown: the stalls depend on how long the idle-gap statistics last into a loaded step, and they end
  at the first analyze inside the step.
- Not shown: which table's statistics matter, `jobs`, `flow_entries` or `enrollments`. They are
  analyzed within seconds of each other. The forced-stale arm only forced `enrollments`, and its early
  steps 200 to 400, whose first pass came 11 to 35 s in, did not stall.
- Not identified: which query regresses.

The next measurement that would name the query is per-step statement statistics or `auto_explain` on
a concurrency 16 run, through step 400.

## 4. Overload: the 5 s pool wait

Two 1.0.39 steps overloaded: `flows-conc16` step 900 and `flows-conc32` step 800. The repeats held
both rates. The 1.0.38 column is concurrency 16 step 800.

| | 1.0.38 c16 step 800 | 1.0.39 c16 step 900 | 1.0.39 c32 step 800 |
|---|---|---|---|
| Trigger generator: accepted / 5xx / transport errors | 82,762 / 0 / 13,237 | 77,827 / 13,997 / 16,174 | 71,726 / 13,210 / 11,062 |
| Background generator: accepted / 5xx / transport errors | 103,452 / 0 / 16,548 | 86,503 / 15,634 / 17,861 | 89,641 / 16,648 / 13,711 |
| API p99 from schedule (ms) | 30,003 | 30,008 | 30,008 |
| Serve slow acquires (over 2 s), longest wait | not recorded | 55,769, 5.0 s | 58,193, 5.0 s |
| Enrollments/s | 406.4 | 450.7 | 497.9 |

Sources: `ramp-1000-900.json` and `ramp-1000-800.json` (generators, status counts), and
`serve-slow-acquires.txt` (serve's `acquired connection, but time to acquire exceeded slow threshold`
warnings, per second). The 1.0.38 column is from `data/tier-l-1.0.38/flows-conc16/flows/ramp-1000-800.json`.

- **The pool wait is capped.** No acquire waited longer than 5.0 s. The requests that could not get a
  connection were refused, so the 5xx counts went from 0 to about 30,000 per step.
- **A refusal takes up to about 10 s.** Each ingest request takes two connections one after the
  other: the API key lookup (`find_api_key_for_auth`, `crates/zirv-flow-api/src/auth.rs`) and the
  inbox append (`append_inbox`, `crates/zirv-flow-api/src/ingest/mod.rs`). Each may wait 5 s. In
  concurrency 16 step 900 the generator's service time was 9.7 s at p95. In the overloaded seconds the
  per-second p50 was 8.4 to 9.9 s, which fits two waits (`timeline_from_schedule` in the generator
  reports, kept in the run's scratch directory and not here).
- **The 30 s transport errors come from the generator host.** In concurrency 16 step 900 at least half
  of the requests scheduled 8 to 10 s into the step timed out at 30 s (per-second p50 30,005 ms). Windows logged these
  Tcpip events (`windows-tcpip-events.txt`; the file has local times, UTC+1):
  - 4227 (local endpoint recently used) at 20:21:28 UTC and 4231 ("all such ports being in use") at
    20:21:45 UTC, both in this step;
  - 4227 at 21:47:07 UTC, 13 s into concurrency 32 step 800;
  - 4227 at 2026-11-25 23:28:16 UTC, 25 s into the 1.0.38 concurrency 16 step 800, the collapse of the
    1.0.38 report.

  The host has 16,384 dynamic TCP ports. The two generators open about 1,900 requests/s, and each
  takes about 9.5 s, so about 18,000 connections are open at once. That is more than the host has
  ports.

So the 1.0.38 collapse came from the server's 30 s pool wait together with the generator host running
out of ports. 1.0.39 shortens the wait and refuses, but at 1,900 requests/s a refusal after 9 to 10 s
still exhausts the ports. Whether a refusal after one 5 s wait would keep the generators under the
limit is not measured. Two overload steps are too few to say whether throughput under overload
improved.

## 5. Capacity against 1.0.38

Background 1,000 events/s. Enrollments/s and trigger-to-enrollment p95 (s).

**Concurrency 16**

| Triggers/s | 1.0.38 | 1.0.39 run 1 | 1.0.39 run 2 |
|---|---|---|---|
| 100 | 97.5, 0.024 | 97.5, 0.023 | 97.5, 0.027 |
| 200 | 189.9, 0.032 | 189.9, 0.032 | 189.9, 0.040 |
| 300 | 277.8, 0.048 | 277.8, 0.047 | 277.8, 0.058 |
| 400 | 354.4, 14.8 (stall) | 355.8, 12.6 (stall) | 354.6, 13.7 (stall) |
| 500 | 442.0, 0.137 | 442.0, 0.137 | 441.9, 0.227 |
| 600 | 517.5, 0.712 | 517.8, 0.387 | 516.0, 1.44 |
| 700 | 563.5, 4.96 | 565.9, 4.62 | 555.0, 6.59 |
| 800 | 406.4, 57.5 (failed) | 592.3, 10.2 | 591.1, 10.4 |
| 900 | not run | 450.7, 13.2 (failed) | 624.7, 14.1 |
| 1,000 | not run | not run | 659.5, 17.4 |

**Concurrency 32**

| Triggers/s | 1.0.38 | 1.0.39 run 1 | 1.0.39 run 2 |
|---|---|---|---|
| 100 | 97.5, 0.024 | 97.5, 0.025 | 97.5, 0.025 |
| 200 | 189.9, 0.034 | 189.9, 0.035 | 189.9, 0.036 |
| 300 | 277.8, 0.079 | 277.8, 0.051 | 277.8, 0.055 |
| 400 | 361.8, 0.069 | 359.7, 6.09 (stall) | 359.7, 5.38 (stall) |
| 500 | 442.0, 0.086 | 442.0, 0.091 | 442.0, 0.091 |
| 600 | 518.0, 0.118 | 518.0, 0.117 | 517.9, 0.128 |
| 700 | 550.9, 31.7 (stall) | 589.8, 0.234 | 589.7, 0.354 |
| 800 | 659.3, 0.744 | 497.9, 3.21 (failed) | 658.1, 1.46 |
| 900 | 710.3, 2.49 | not run | 593.1, 21.9 |
| 1,000 | no result (crash) | not run | 630.5, 19.4 |

**Concurrency 4** (`flows-conc4`). In each ramp, the last held step and the highest step under 1 s:

| Background | 1.0.38 | 1.0.39 |
|---|---|---|
| 0 | held 400; under 1 s to 300 (step 250 stalled, 12.6 s) | held 450, the maximum; under 1 s to 300 (step 250 stalled, 9.4 s) |
| 1,000 | held 300; under 1 s to 200 | held 350; under 1 s to 150 (step 200 stalled, 9.2 s; 250 at 6.85 s) |
| 2,000 | held 350; under 1 s to 150 | held 300; under 1 s to 150 (step 200 stalled, 8.8 s) |

- **Equal steps cost the same.** The rates both versions held matched 1.0.38's enrollments/s within
  2%, except at stall steps. PostgreSQL CPU matched within 0.6 cores, about as much as the two 1.0.39
  runs differ from each other. In the held steps from 700 triggers/s up, PostgreSQL averaged 7.1 to
  7.5 of its 8 cores (`containers` in the step files). The person-first index
  costs nothing measurable.
- **The concurrency 4 run at 2,000 events/s is not comparable.** Its ramp started with 448,681
  completed enrollments, against 324,435 in 1.0.38 pass 2, because this run's 1,000 events/s ramp ran
  7 steps instead of 3. It held one step less, and enrollments/s were 7 to 9% lower at 200 to 300
  triggers/s. The data does not separate the table size from the version.
- **The practical limit is not better.** That is the highest step under 1 s, the measure used in the
  1.0.38 report. The stall steps move with the autovacuum phase from run to run, and they still set
  this limit.

## 6. PostgreSQL failures in the metrics query

- At 2026-11-26T20:48:42Z a backend running `scripts/bench/flows/metrics.sql` died with signal 11.
  This was at the end of `flows-conc16-stale` step 700. The postmaster restarted, and 158 connection
  attempts were refused during recovery (`flows-conc16-stale/flows/postgres-crash.txt`).
- At 2026-11-27T00:23:27Z the same query failed with `ERROR: pfree called with invalid pointer
  0x7a1fd5a6dbb8 (header 0x0000000000000740)`. This was at the end of `flows-conc32-repeat` step
  1,100 (`flows-conc32-repeat/flows/postgres-crash.txt`).

Each lost its step. With the 1.0.38 concurrency 32 crash at step 1,000 that makes three failures. All
three were in this one query, in the heaviest steps of their runs, and none in a product statement.
The System log has no WHEA-Logger event since 2026-11-19.

The 1.0.38 report left a CPU fault on this host open as a cause. The metrics query runs for a few
seconds after each 120 s step, while the product's statements keep PostgreSQL busy through the whole
step. Random faults would rarely hit that one query three times out of three, so the repetition makes
a random hardware fault less likely. The cause is still not established.

## 7. Raw data

Every run directory `data/tier-l-1.0.39/<run>/flows/` has these files:
- `stack.txt` (image, version, concurrency, pool sizes) and `diag-logging.txt`;
- `seed*.json`, `flow-definition.json`, the window files and the `ramp-*` files;
- `autovacuum.txt`: every autovacuum and autoanalyze of a `public` table, one line each, in UTC;
- `worker-slow-statements.txt`: the worker's statements over 1 s, as time, seconds, statement;
- `postgres-lock-waits.txt`: lock waits over 1 s.

`flows-conc4` has neither of the last two, because its worker logged no slow statement and PostgreSQL
logged no lock wait.

`serve-slow-acquires.txt` is in the two runs with overload steps, and `postgres-crash.txt` is in the
two runs that lost a step. `windows-tcpip-events.txt` at the top holds the port-exhaustion events. The
containers' full logs and the generator reports stayed in the run's scratch directory and are not
kept.
