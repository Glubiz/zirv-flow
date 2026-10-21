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
  otherwise; `/dev/shm` 1 GB, as `compose up` recreated the container with the script default).
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
  each) and the shape against the sample, 23 of 23 checks OK per run (`data/m5/seed-verify-run*.txt`).
  The tables were vacuumed and analysed and a checkpoint taken after seeding; seeding finished at
  least 30 minutes before the window. Rows were deleted between runs, so each run started from the same
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
differ from #151's runs), so it is evidence for #151, not a new finding.

## 4. Misses and causes

1. **Timer lateness under ingest load: p99 136.4 s against 60 s** (run 3). Established: the wake jobs
   and the ingest work share the PostgreSQL instance and the worker pool. With 2,000 events/s the
   database runs at 480% CPU on average against 82% without load, and the worker's peak claim rate falls
   from 1,108 to 832 jobs/s, so the 1,667/s burst backlog grows to 91,000 and takes about 3 minutes to
   drain. Lateness is also 8 to 15 s at p99 in the minutes outside the burst, where no load-free minute
   exceeds 1 s. Not established: how much comes from ingest-side jobs holding the 4 worker slots and how
   much from database contention; the sampler records the queue as a whole, not by job kind. Filed as #207.
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
