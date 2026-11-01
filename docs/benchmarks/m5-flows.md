# M5 timer benchmark

Issue #169, exit criterion 8 of milestone 5: architecture section 19, row "Waiting enrollments; timer
lateness" (p99 < 60 s; M5 measures at 1 million waiting enrollments, M8 at 10 million). Measured on the
published image `ghcr.io/glubiz/zirv-flow:0.76.0` (digest
`sha256:069498001c4a37d17ac22cb960cff0019abf1d00bf0a51d9e678473a8fbfd66b`). Raw data is in
[`data/m5/`](data/m5/); the scripts are in `scripts/bench/m5/` (runbook: `scripts/bench/README.md`
section 10).

**This is a development machine, not reference hardware, and the results are provisional until the M8
hardware run.** The ingest figures sit beside the M4 numbers of #151.

**This is a timer benchmark. The 1,000,000 waiting enrollments were seeded by SQL, so it gives no
evidence of enrollment throughput** (supervisor ruling 46fb269a).

## 1. Summary

| Run | Ingest load | Lateness p99, all 1 M | Burst set p99 | Max | Target p99 < 60 s |
|---|---|---|---|---|---|
| 1 | none | 52.4 s | 55.3 s | 55.8 s | HIT (thin) |
| 2 (repeat of 1) | none | 51.8 s | 54.5 s | 55.1 s | HIT (thin) |
| 3 | 2,000 track events/s for the whole window | 136.4 s | 140.6 s | 141.7 s | **MISS** |

Without ingest load the target is met, with a thin margin that the burst alone uses: the worker
drains about 1,050 to 1,160 wake jobs per second, the burst arrives at about 1,670 per second for one
minute, and the backlog (57,000 jobs, oldest 54 s) is the whole of the lateness tail. At 2,000 events/s
the same worker drains 830 jobs/s at best, the backlog reaches 91,000 and lateness 142 s. One miss was
filed (section 5).

## 2. Method

- **Stack.** The M4 10 M-people database (compose project `zirv-flow-m4bench10m`, volumes kept from
  M4, migrated from 0.57.1 to 0.76.0 by the image), app tier M (4 vCPU cpuset `0-3` for serve, worker
  and scheduler; worker concurrency 4), PostgreSQL 16.15 on cpuset `8-31` (`shared_buffers=4GB`, stock
  otherwise; `/dev/shm` 1 GB, as `compose up` recreated the container with the script default; M4 ran with 8 GB, `BENCH_PG_SHM=8gb`).
  Host: 13th Gen Intel Core i9-13900K, Docker Desktop 29.2.1 (32 CPUs, 31 GiB), WSL 3.0.3.0
  (`data/m5/stack.txt`). No `cargo.exe` ran during any window (`cargo.log` in each run directory).
- **Flows (through the API).** `m5-spread`: manual trigger, `wait.duration` (86,400 s) then exit.
  `m5-burst`: `wait.until_time` (a local date and time) then exit. `m5-probe`: `wait.duration` 20 s.
- **Sample through the product path.** A manual start for a list of 3 people (probe: 5) on the 0.76.0
  image; the rows the runtime left were captured (`data/m5/sample/*.json`): per enrollment one
  `enrollments` row (state `waiting`, node `w1`, `state_version` 1, `wake_at`), one `step_executions`
  row (`w1`, `waiting`), one `flow.step` job (`input: wake`, `run_at = wake_at`, unique key
  `wake:<enrollment>:1`), one `flow_entries` row and three `decision_records`. The five probe
  enrollments woke and completed 20 s later within about 45 ms of their wake time.
- **Seed (SQL).** Rows of exactly that shape, copying every column of the sample row
  (`jsonb_populate_record`) and changing only ids, person, trace and instants: 900,000 `m5-spread`
  enrollments with `wake_at` uniform over a 40 minute window (375 per second), and 100,000 `m5-burst`
  enrollments waking in one minute, 20 minutes into the window (1,667 per second on top). Validated by
  `verify.sql` before each window: all counts (enrollments, steps, ready jobs, entries, 3 decisions
  each) and the shape against the sample, 23 of 23 checks OK per run (`data/m5/seed-verify-run*.txt`). `flow_entries` was checked by count, not column by column.
  The tables were vacuumed and analysed and a checkpoint taken after seeding; verify.sql refuses to pass when the window is less than `SEED_MIN_MARGIN_S` (120 s by default) away. Rows were deleted between runs, so each run started from the same
  table sizes. Not seeded: the start job (deleted by the runtime when it completes).
- **Lateness** = `step_executions.started_at` of the step after the wait (the transaction start of the
  wake job) minus the seeded wake instant, nearest-rank percentiles over all 1,000,000 timers. The
  wake instant is kept in `m5bench.rows` because the runtime clears `wake_at` on completion.
- **Claim rate, queue depth, CPU.** A sampler logged every 5 s the due ready jobs, running jobs, the
  oldest due job's age and the `jobs` counters (claims are the `n_tup_upd` delta, an upper bound as it
  also counts lease renewals; the completion rate agreed within 0.1%), and every 10 s `docker stats`.
- **Ingest load** (run 3): `tools/loadgen`, track mode, 2,000 events/s for 2,400 s over 100,000
  people, seed 77, started at the window start. Run 2 was meant to be this run but was started without
  the load by a script mistake (label `load` with no mode); it is reported as a second no-load run,
  and `measure.sh` now refuses that combination.

## 3. Results

Lateness in seconds (nearest rank). The spread set includes the 22,000 spread timers that fall in the
burst's minute; they are listed separately.

| Run | Set | n | p50 | p95 | p99 | max |
|---|---|---|---|---|---|---|
| 1 no load | all | 1,000,000 | 0.52 | 37.8 | 52.4 | 55.8 |
| | spread | 900,000 | 0.45 | 9.78 | 46.7 | 55.8 |
| | spread outside the burst minute | 877,646 | 0.43 | 0.99 | 40.8 | 55.8 |
| | spread inside the burst minute | 22,354 | 28.6 | 53.2 | 55.3 | 55.8 |
| | burst | 100,000 | 28.2 | 53.1 | 55.3 | 55.8 |
| 2 no load | all | 1,000,000 | 0.54 | 37.2 | 51.8 | 55.1 |
| | spread | 900,000 | 0.47 | 9.78 | 46.0 | 55.1 |
| | burst | 100,000 | 27.7 | 52.6 | 54.5 | 55.1 |
| 3 loaded | all | 1,000,000 | 9.32 | 115.5 | 136.4 | 141.7 |
| | spread | 900,000 | 8.62 | 100.7 | 133.2 | 141.7 |
| | spread outside the burst minute | | 8.45 | 98.9 | 131.8 | 141.7 |
| | burst | 100,000 | 74.3 | 135.8 | 140.6 | 141.7 |

Per-minute figures are in `lateness.json` of each run. Without load every minute outside the burst
has p99 about 1 s (the poll interval) until the backlog drains; no timer fired early; no wake job
died.

| | Run 1 | Run 2 | Run 3 (loaded) |
|---|---|---|---|
| Mean claim rate over the run | 404/s | 403/s | 408/s |
| Peak claim rate (10 s mean) | 1,108/s | 1,162/s | 832/s |
| Deepest due queue / oldest due job | 57,311 / 54.5 s | 55,974 / 54.2 s | 91,205 / 141.7 s |
| PostgreSQL CPU, mean / max (percent of one core) | 82 / 237 | 77 / 253 | 480 / 714 |
| Worker CPU, mean / max | 34 / 97 | 31 / 97 | 123 / 235 |
| Serve CPU, mean / max | 0.1 / 1.5 | 0.1 / 1.6 | 122 / 199 |

Ingest API under load (run 3, 4,800,000 requests, all 202, achieved 1,999.99/s):

| | p50 | p95 | p99 | max |
|---|---|---|---|---|
| Server-side `service_latency` | 3.6 ms | 375.7 ms | **1,015.7 ms** | 2,970 ms |
| `latency_from_schedule` | 4.8 ms | 379.1 ms | **1,019.6 ms** | 2,971 ms |

Beside #151 (target p99 < 100 ms; 0.56.0: 57.7 ms, 0.57.1: 461 to 635 ms server-side at 1 M people):
1,016 ms on 0.76.0, during timer wakes, on the 10 M database, with the loadgen dispatch lag p99 at
9.4 ms. This run does not isolate the cause (the wake load, the database state and the version all
differ from #151's runs), so it is evidence for #151, not a new finding. The stacks also differ in `/dev/shm` (1 GB here, 8 GB in M4).

## 4. Misses and causes

1. **Timer lateness under ingest load: p99 136.4 s against 60 s** (run 3). Established: the wake jobs
   and the ingest work share the PostgreSQL instance and the worker pool. With 2,000 events/s the
   database runs at 480% CPU on average against 82% without load, and the worker's peak claim rate falls
   from 1,108 to 832 jobs/s, so the 1,667/s burst backlog grows to 91,000. After the burst (minute 20) the spread set's p99 stays above 50 s through minute 25 (about 6 minutes) and returns to about 16 s at minute 28, about 8 minutes after the burst. Before the burst the per-minute p99s range from 0.07 to 15.9 s; after recovery (minutes 28 to 39) they stay at 13 to 15 s, against about 1 s in every minute outside the burst without load. Not established: how much comes from ingest-side jobs holding the 4 worker slots and how
   much from database contention; the sampler records the queue as a whole, not by job kind. Filed as #207;
   the claim-plan cause found later and its fix are in section 8.
2. The no-load runs meet the target with a thin margin. The burst backlog alone produces the whole tail (57,000 jobs, 54 s): the backlog grows roughly in proportion to the burst, so one about 10% larger in the same minute would miss at this tier even unloaded (an estimate, not measured). Not extrapolated to 10 M.

## 5. Issues filed

Miss 1: [#207](https://github.com/Glubiz/zirv-flow/issues/207).

## 6. Caveats

- Seeded timers, not enrolled: no evidence of enrollment throughput; the wake path is the product's.
- Seeded decisions and entries have no history before the window; the start job is not seeded.
- The 40 minute window gives 375 spread wakes per second; a 10 M population over a day would be about
  116 per second. The burst is the stress.
- Same database and host for all runs; run 3 followed two runs and two deletes of 1 M rows (vacuumed).
- Docker Desktop on a workstation; other processes may have used the host (no `cargo.exe` did).

## 7. Reproduce

```sh
docker pull ghcr.io/glubiz/zirv-flow:0.76.0
cargo build --release -p zirv-flow-loadgen
scripts/bench/m5/stack.sh up && scripts/bench/m5/setup.sh && scripts/bench/m5/sample.sh
RUN=1 M5_T0_LEAD_MIN=30 scripts/bench/m5/seed.sh stage load vacuum verify
scripts/bench/m5/measure.sh start 1 noload          # or: start 3 loaded load
scripts/bench/m5/measure.sh wait 1 noload && scripts/bench/m5/measure.sh collect 1 noload
RUN=1 scripts/bench/m5/seed.sh clean && scripts/bench/m5/stack.sh stop
```

`shellcheck` was not installed; `bash -n` passes for every script.

## 8. Cause and fix (#207)

The capped A/B of draft PR #472 (branch `bench/m8-cause-ab`, data `docs/benchmarks/data/cause/207/` there;
100,000 timers, a burst of 10,000 at 300 s, a fresh database per run) showed that ingest load alone does
not explain the lateness: with no ingest, run E1 reached p99 412.7 s and run E2, the same setup, 1.0 s;
with 500 events/s, F1 3.5 s and F2 1.6 s. The claim statement was then measured alone.

**Method.** `jobs::tests::the_claim_cost_against_a_timer_backlog` (`crates/zirv-flow-store/src/jobs.rs`)
seeds `flow.step` wakes due 10 to 40 minutes ahead, a burst due in the last minute, and other work
(ingest drains, enrolments, webhook calls at priority 5, maintenance at priority -1 an hour ahead, a few
running jobs). Autovacuum is off on `jobs`, so the statistics change only where the test analyses. For
each statistics state it runs the statement before #207 and the new one on the same rows: the plan a
connection settles on after six executions, by `EXPLAIN (ANALYZE, BUFFERS)`, then four workers that claim
and complete for 5 s with the worker's 33 kinds. PostgreSQL 16.15, stock settings, a development
machine, the debug test build: about 1,500 claims/s is that client's ceiling, so the rates compare the
two statements, not the product.

**Cause.** The claim was one `ORDER BY priority, run_at LIMIT 1 FOR UPDATE SKIP LOCKED` over the due
ready jobs, with two plans, both growing with the queue:

1. `jobs_ready_kind_idx` and a sort of every due job, on every claim (the sort is not even top-N, since
   the row lock sits between it and the limit, and spills to disk at 100,000 rows). The planner takes it
   when the statistics put almost nothing due. `run_at <= now()` is estimated from the `run_at`
   histogram, and statistics taken while every timer still lay ahead (the autovacuum `ANALYZE` after the
   timers are armed) put the due set at the histogram's floor, 0.01% of the table. The plan holds until
   the next `ANALYZE`, which autovacuum runs after 2% of the table has changed, so whether a run stalls
   depends on when autovacuum ran. That is consistent with E1 against E2 (their plans were not captured).
   At 1.1 M rows the floor is 110 rows, enough to keep plan 2, which fits the unloaded 1 M runs of
   section 3 not stalling; at 100,000 timers it is 10 to 20 rows, and the planner sorts.
2. `jobs_ready_idx` with `run_at <= now()` as a condition on its second column only. It stops at the
   first due job only when one sits at the lowest priority in use; an empty claim, or a claim of a job at
   a later priority, steps over every future job ahead of it.

**Fix.** The claim walks the distinct priorities of the ready jobs upwards, one probe of `jobs_ready_idx`
each (a recursive skip scan), and takes the oldest due job of the first band that has one from the
bounded range `priority = band AND run_at <= now()`, `FOR UPDATE SKIP LOCKED`. The semantics are kept
(priority first, then oldest; locked jobs skipped, also across bands) and the enqueue wake check of 0070
is unchanged. Without a cap no index is added: `jobs_ready_idx` already has the order. The per-workspace
cap kept its semantics but not a bounded cost: under a cap the band probe still stepped over a capped
workspace's due jobs, which the cap path below fixes.
`jobs::tests::the_claim_plan_stays_bounded_whatever_the_statistics` asserts the plan has no sort (but of
the capped claim's per-workspace heads), no kind index and no ready index scan unbounded by priority or
workspace, with no statistics, with statistics taken while nothing was due and with fresh ones, as custom
plans of the claim without and with a cap and as their generic plans; it also asserts that the statement
before #207 fails that check in every case.

**Before and after.** Per claim: the `EXPLAIN (ANALYZE, BUFFERS)` execution time and shared buffers of the
plan the connection settled on; rates from the four workers.

| Waiting / due | Statistics | Before: plan | ms, buffers | Claims/s | After: ms, buffers | Claims/s |
|---|---|---|---|---|---|---|
| 100,000 / 103,008 | taken while nothing was due | kind index, sort of 103,002 rows | 53.1, 100,855 | **70** | 0.12, 52 | 1,476 |
| 100,000 / 23,008 | taken while nothing was due | kind index, sort of 23,002 rows | 10.3, 20,283 | **319** | 0.10, 66 | 1,493 |
| 100,000 / 95,000 to 103,000 | none; taken with drains due; fresh | `jobs_ready_idx` | 0.07 to 0.18, 30 to 81 | 1,446 to 1,481 | 0.13 to 0.15, 119 to 215 | 1,461 to 1,508 |
| 1,000,000 / 88,000 to 103,000 | none; with drains due; nothing due; fresh | `jobs_ready_idx` | 0.07 to 0.09, 30 to 111 | 1,433 to 1,482 | 0.11 to 0.17, 110 to 285 | 1,354 to 1,466 |
| 1,000,000 / 1,000 at priority 5 | fresh | `jobs_ready_idx` from the start | 7.24, 4,875 | 200, and 221 empty | 0.09, 52 | 200, and 3,500 empty |
| 1,000,000 / none | fresh | `jobs_ready_idx`, whole | 7.24, 4,865 | 462 empty | 0.06, 30 | 4,092 empty |

The old sort grows linearly with the due set (10.3 ms at 23,002 due, 53.1 ms at 103,002), so a burst
drains in quadratic time while that plan holds. The new claim costs 0.05 to 0.17 ms in every case; where
a due job sits in the first band it reads that band's head twice (to find the band and to claim from it),
hence its somewhat higher buffer count there. Raw output was not committed; the test reprints it.

**Not established.** The loaded 1 M run of section 3 (p99 136.4 s): at 1.1 M rows the old claim kept
plan 2 under every statistics state tried here, and plan 2 claims a priority-0 job cheaply while one is
due, so the drain-rate drop under load that section 4 records is not explained by these measurements.
Plan 2's empty claim cost 7 ms at 1 M waiting timers (0.06 ms now); every worker that an enqueue wakes for
a job another worker already took pays it, which may account for part of the database CPU under load,
not measured on the stack. The fix is verified on the claim statement; the timer benchmark itself is to
be re-run on the M8 hardware.

```sh
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres ZIRV_FLOW_BENCH_CLAIM=1 \
  ZIRV_FLOW_BENCH_CLAIM_FUTURE=100000 ZIRV_FLOW_BENCH_CLAIM_DUE=100000 \
  cargo nextest run -p zirv-flow-store --run-ignored all --no-capture \
  -E 'test(the_claim_cost_against_a_timer_backlog)'
```

(The defaults are 1,000,000 and 100,000.)

**Cap path.** With `worker.max_running_per_workspace` set, the band probe above also counted the running
jobs of each row's workspace and skipped a capped workspace's rows one by one, so a workspace at its cap
with a flood of due jobs made every claim step over the whole flood, counting for each row, before it
reached another workspace's job, or return nothing. That is the case the queue-backlog runbook (cause 3,
fix 3) sets the cap for. The capped claim now reads the capped workspaces once
(`jobs_workspace_running_idx`, grouped, `HAVING count(*) >= cap`) and, while there are none, is the claim
above. Otherwise it walks `jobs_ready_workspace_idx (workspace_id, priority, run_at)` one workspace and
band at a time (a skip scan): a capped workspace is passed over in one probe, and each other workspace
yields the oldest due job of its first band that has one. Instance jobs, never capped, are walked by band
in `jobs_ready_instance_idx (priority, run_at) WHERE workspace_id IS NULL`. The heads are tried by
priority, then age, `FOR UPDATE SKIP LOCKED`, so a claim locks only the row it takes; a head that is
locked is passed over until the next claim, where the claim above would try that workspace's next job.
Migration 0088 adds both indexes; each ready job is in one of them, so an enqueue writes one index entry
more. An index on `(priority, workspace_id, run_at)` was tried first: with no statistics the planner
put every band at one row and answered the probes from `jobs_ready_idx` with a filter or a sort, which
steps over the flood again. Leading with the workspace, with every workspace probe a row comparison led
by `workspace_id`, leaves no other index able to answer them.
`jobs::tests::the_capped_claim_plan_does_not_step_over_a_capped_workspace` asserts, with no statistics,
with statistics taken while nothing was due and with fresh ones, as the custom and the generic plan, that
no ready index scan filters on the workspace and no running count is taken per row, and that the claim
before this change fails that check in every case.

`jobs::tests::the_capped_claim_cost_against_a_workspace_flood` puts a workspace at a cap of 10 running
jobs with a flood of `flow.step` jobs due two to three minutes ago, ahead of eight other workspaces'
20,000 jobs due in the last minute in the same band, with fresh statistics. The claim before and after
run as `claim_job` runs them under the cap, on the same rows (the other workspaces' jobs are seeded again
before each run): the plan a connection settles on, measured as above, and four capped workers for 5 s.

| Flood | Due besides it | Before: ms, buffers | Claims/s | After: ms, buffers | Claims/s |
|---|---|---|---|---|---|
| 10,000 | 20,000 of 8 workspaces | 15.0, 20,053 | 77 | 0.22, 127 | 718 |
| 10,000 | none | 16.5, 20,028 | 84 empty | 0.10, 23 | 1,397 empty |
| 100,000 | 20,000 of 8 workspaces | 166.7, 200,518 | 8.2 | 0.19, 129 | 731 |
| 100,000 | none | 165.7, 200,493 | 8.1 empty | 0.09, 24 | 1,455 empty |
| 1,000,000 | 20,000 of 8 workspaces | 2,594, 2,004,792 | 0.5 | 0.27, 279 | 698 |
| 1,000,000 | none | 2,569, 2,004,768 | 0.5 empty | 0.10, 24 | 1,429 empty |

The claim before grew linearly with the flood, two buffers per flooded row; after, it reads about two
probes per workspace whatever the flood, in 0.19 to 0.27 ms (its buffers rose from 127 to 279 at
1,000,000, per probe, not per flooded row). With the flood's workspace below its cap, so that no
workspace is capped, both claim the flood at 738 to 764 claims/s in 0.11 to 0.14 ms (the run before
leaves about 4,000 fewer flood jobs for the run after): about 750 claims/s is this client's ceiling for
claims serialised by the cap's lock, and the check for capped workspaces adds at most 0.02 ms. The cost
after grows with the workspaces that have ready jobs, which this benchmark keeps at nine.

```sh
DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres ZIRV_FLOW_BENCH_CLAIM=1 \
  ZIRV_FLOW_BENCH_CLAIM_FLOOD=1000000 \
  cargo nextest run -p zirv-flow-store --run-ignored all --no-capture \
  -E 'test(the_capped_claim_cost_against_a_workspace_flood)'
```

(The default is 100,000.)
