# Tier L capacity benchmark: ingestion and flow execution

How much load one Tier L deployment (8 CPUs and 16 GB for the application, PostgreSQL on its own 8
CPUs) can take in through the ingest API, and how fast flows run on it at production-sized loads.
Measured on the published image `ghcr.io/glubiz/zirv-flow:latest`, version 1.0.38 (id
`sha256:55ad1409e0b0b40192bb6aa837e07fdefa286434701c5dd1b0b9ecb73ef06f55`, created
2026-11-24T14:38:45Z). Raw data is in [`data/tier-l-1.0.38/`](data/tier-l-1.0.38/); the scripts are
`scripts/bench/tier.sh` (ingest) and `scripts/bench/flows.sh` (flows; `scripts/bench/README.md`
section 8, "Flow execution under ingest load").

**This is a development machine, not reference hardware.** Host: 13th Gen Intel Core i9-13900K,
Windows 11, Docker Desktop 29.2.1 (32 CPUs, 31 GiB VM). The load generator (`tools/loadgen`, open
loop, latency measured from the scheduled send time) runs on the host, outside the budget.

## 1. Summary

The questions were how much load the system can handle and how slow flow execution is at normal
production loads. This report does not know the production load. It gives rates with their daily
equivalents: 100 flow triggers per second is 8.64 M flow entries per day, and 1,000 events per second
is 86.4 M events per day.

**Ingestion.** With the serve database pool at 32 connections (the value `docs/operations/sizing.md`
recommends for serve) the API accepts 4,250 track events/s, 4,500 identify items/s and 8,000 batch
items/s without errors. At the product default pool of 10 the limits are 2,750, 2,500 and 8,000.
Applied latency (arrival to applied) stays under 100 ms at p95 up to 4,250 track events/s and 2,750
identify items/s at pool 32. For identify the API still accepts 4,500/s, but applied latency is then
seconds.

**Flow execution.** At production-sized load (a 1-hour soak at 100 triggers/s plus 1,000 events/s of
background ingest, run at the default worker concurrency 4) a person is enrolled 15 ms after the
trigger event at p50 and 40 ms at p99. The flow, without its 30 s wait, finishes in 35 ms at p50 and
108 ms at p99. The wake after the wait is 5 ms late at p50 and 29 ms at p99. With 1,000 events/s of
background load and concurrency 4 the practical limit is 200 triggers/s (190 enrollments/s); beyond it
a backlog builds in the job queue.

**Worker concurrency caps flows on Tier L.** The default `worker.concurrency` of 4 is what limits flow
throughput here: the worker uses about 2 of its 8 cores at the knee. Raising it to 16 or 32 lifts the
practical limit at 1,000 events/s of ingest from 200 to 600 or 800 triggers/s (51.8 M or 69.1 M flow
entries per day), and trigger-to-enrollment at 50 triggers/s stays the same (`bg1000` at concurrency
4, 16 and 32, section 5). After that PostgreSQL's 8 cores are the limit: it reaches 7 to 8 cores while
the worker averages 1.8 to 2.8. The soak above is the production-load latency answer and ran at concurrency 4; it was
not repeated at 16 or 32. Section 5 has the data and the caveats (stalls at c16 step 400 and c32 step
700, and a collapse at c16 step 800). [The 1.0.39 rerun](tier-l-1.0.39-rerun.md) re-measures both
after the fixes.

| Question | Result | Source |
|---|---|---|
| Track ingest, accepted ceiling, pool 10 / pool 32 | 2,750 / 4,250 events/s | `L/track1-ceiling.json`, `serve-pool-32/L/track1-ceiling.json` |
| Track ingest, applied p95 under 100 ms, pool 10 / pool 32 | 2,750 / 4,250 events/s (p95 20.4 / 44.0 ms) | `L/track1-steps.jsonl`, `serve-pool-32/L/track1-steps.jsonl` |
| Identify ingest, accepted ceiling, pool 10 / pool 32 | 2,500 / 4,500 items/s | `L/identify1-ceiling.json`, `serve-pool-32/L/identify1-ceiling.json` |
| Identify ingest, applied p95 under 100 ms, pool 10 / pool 32 | 2,500 / 2,750 items/s (p95 38.6 / 47.9 ms) | `L/identify1-steps.jsonl`, `serve-pool-32/L/identify1-steps.jsonl` |
| Batch ingest (100 items per request), accepted ceiling, pool 10 / pool 32 | 8,000 / 8,000 items/s | `L/batch1-ceiling.json`, `serve-pool-32/L/batch1-ceiling.json` |
| Batch ingest, applied p95 under 100 ms, pool 10 / pool 32 | 6,000 / 6,000 items/s (p95 48.5 / 39.6 ms) | `L/batch1-steps.jsonl`, `serve-pool-32/L/batch1-steps.jsonl` |
| Flow enrollments/s at the last step passing the 60 s criterion, background 0 / 1,000 / 2,000 events/s | 330 (at 400 triggers/s) / 229 (at 300) / 243 (at 350) | `flows-run2/flows/ramp-0.json`, `flows/ramp-1000.json`, `flows-run2/flows/ramp-2000.json` |
| Practical flow limit (highest triggers/s step with trigger-to-enrollment p95 under 1 s), background 0 / 1,000 / 2,000 events/s | 300 (200 if the stalled step 250 is counted) / 200 / 150 triggers/s | the same three ramp files |
| Practical flow limit at 1,000 events/s background, worker concurrency 4 / 16 / 32: highest step under 1 s | 200 / 600 / 800 triggers/s (17.3 M / 51.8 M / 69.1 M flow entries per day) | `flows-run2/flows/ramp-1000.json`, `flows-conc16/flows/ramp-1000.json`, `flows-conc32/flows/ramp-1000.json` |
| The same, as the highest step below which every step is under 1 s | 200 / 300 / 600 triggers/s | the same files; c16 step 400 and c32 step 700 stalled (section 6) |
| Enrollments/s, highest held step at 1,000 events/s background, concurrency 4 / 16 / 32 | 229 (at 300) / 563 (at 700) / 710 (at 900; the ramp stopped there, not at a limit) | the same files |

The 60 s trigger-to-enrollment p95 criterion of `flows.sh` is lenient: it passes steps where a person
waits 19 s to be enrolled. The practical limit in the table is the highest step whose
trigger-to-enrollment p95 stays under 1 s. That is a limit chosen for this report, not a pass
criterion of the script. 200 triggers/s is 17.3 M flow entries per day; 150/s is 13.0 M; 300/s is 25.9 M.

Flow speed at production-sized load (seconds; trigger-to-enrollment, end-to-end minus the 30 s wait,
wake lateness):

| Window | Triggers/s, background events/s | Duration | Enrollments | trig-to-enroll p50 / p95 / p99 | end-to-end minus wait p50 / p95 / p99 | wake lateness p50 / p95 / p99 |
|---|---|---|---|---|---|---|
| `soak60-100` (pass 2) | 100, 1,000 | 3,600 s | 349,557 | 0.015 / 0.027 / 0.040 | 0.035 / 0.060 / 0.108 | 0.005 / 0.015 / 0.029 |
| `soak-150` (pass 1) | 150, 1,000 | 900 s | 129,292 | 0.018 / 0.043 / 0.071 | 0.046 / 0.109 / 0.675 | 0.006 / 0.025 / 0.611 |
| `idle` (pass 1) | 50, 0 | 300 s | 14,804 | 0.007 / 0.014 / 0.017 | 0.020 / 0.503 / 0.872 | 0.005 / 0.477 / 0.833 |
| `bg1000` (pass 1) | 50, 1,000 | 300 s | 14,783 | 0.013 / 0.022 / 0.031 | 0.031 / 0.518 / 0.886 | 0.007 / 0.481 / 0.829 |
| `bg1000-repeat` (pass 2) | 50, 1,000 | 300 s | 14,780 | 0.013 / 0.023 / 0.031 | 0.032 / 0.511 / 0.881 | 0.007 / 0.475 / 0.823 |
| `bg2000` (pass 1) | 50, 2,000 | 300 s | 14,785 | 0.017 / 0.028 / 0.038 | 0.038 / 0.520 / 0.891 | 0.008 / 0.475 / 0.827 |

Sources: the window JSON files named in the first column (`flows/` for pass 1, `flows-run2/flows/` for
pass 2), key `latency_s`. All windows had 0 failed enrollments and 0 dead jobs. In the 50 triggers/s
windows wake lateness has a tail of 0.5 to 0.8 s at p95 and p99, which depends on the window length (section 4.3).

CPU at the knees (cores; average / peak during the generators, from `containers` in the step files):
PostgreSQL 5.76 / 6.56 and worker 1.81 / 2.32 at 1,000 events/s background and 250 triggers/s;
PostgreSQL 3.91 / 4.98 and worker 1.82 / 1.91 at no background and 400 triggers/s; PostgreSQL
6.53 / 6.81 and worker 2.00 / 2.16 at 2,000 events/s background and 350 triggers/s. At worker
concurrency 4 the worker uses about 2 of its 8 cores while the job queue backs up; PostgreSQL uses 3.9
to 6.5 on average (peak 5.0 to 6.8). Section 5 gives the same measurements at worker concurrency 16 and
32.

## 2. Method

- **Stack.** Compose project `zirv-flow-bench`. Tier L: serve, worker and scheduler share cpuset 0-7
  (8 logical CPUs); memory limits serve 6554m, worker 8192m, scheduler 1638m (16 GB in total).
  PostgreSQL 16.16 has its own cpuset 8-15 and a 16 GB limit: `shared_buffers` 4GB,
  `effective_cache_size` 16GB, `maintenance_work_mem` 1GB, `max_wal_size` 8GB,
  `checkpoint_timeout` 15min, otherwise stock. Facts per run are in `stack.txt`, `*-stack.txt` and
  `*-pg-facts.txt`. `worker.concurrency` was 4 (the product default) in every ingest run and in flow
  passes 1 and 2; ingest drain shards 8.
- **Serve database pool.** The product default `database.max_connections` is 10. Phase A ran at 10
  (`L/`). The track, identify and batch ramps were then re-run at 32 (`serve-pool-32/L/`). Flow passes 1
  and 2 ran at 32 (`ZIRV_FLOW_DATABASE__MAX_CONNECTIONS=32` in `flows/stack.txt`).
- **Ingest ramps** (`tier.sh`). Steps of 90 s at a fixed open-loop rate. Before step 1: a 90 s
  warm-up (at the first step's rate unless the ceiling file records a `warmup_rate`; 1,000/s for the
  pool 32 track ramp), then `ANALYZE`. A fresh database otherwise plans on empty-table statistics and
  stalls (section 3.4). A step fails on API p99 from schedule at or above 100 ms, any 5xx or 429,
  generator late dispatch or achieved rate below target, inbox age at or above 5 s or growing, or
  transport errors above 1e-4 of accepted requests. Docker Desktop's Windows port proxy drops an
  isolated connection now and then; the tolerance covers that, and `L/strict/` holds zero-tolerance
  runs for comparison. The pool 10 batch ramp ran without the tolerance (`transport_tolerance` null in
  `L/batch1-ceiling.json`) and had 0 transport errors in every step. Track and identify send one item per request; batch sends 100 items per
  request. Ramp start and step: track pool 10 from 2,250 by 250; identify pool 10 from 1,250 by 250;
  track pool 32 from 2,750 by 250; identify pool 32 from 2,000 by 250; batch from 2,000 by 2,000
  (both pools).
- **Applied latency** (`scripts/bench/applied-latency.sql`). Track events (also used for batch): the
  time the drain minted the event id minus `events.received_at`, that is, inbox arrival to applied.
  Identify: the last `trait.change` decision of a source record minus the observation time the record
  keeps. Measured from the database after each step.
- **Comfortable rate** in section 1 is the highest step whose applied p95 is under 100 ms and that
  also passed the API criteria. It is a definition of this report.
- **Flow benchmark** (`flows.sh`). 100,000 people seeded by identify. One published flow, triggered by
  the event `bench_flow_trigger`: branch on `plan == pro`, then update a trait or add to a list, then
  `wait.duration` 30 s, then increment a trait, then exit (`flows/flow-definition.json`). Re-entry is
  `once_at_a_time`: a trigger for a person already in the flow is refused `already_active`. That is
  expected; the refusal counts are under `counts` in each window file. Flow warm-up is off. Each
  window runs a background track load at one rate and trigger events at another for N seconds, then
  waits for all enrollments to finish. Ramps use 120 s per step. A step fails on trigger-to-enrollment
  p95 above 60 s, enrollments below 95 percent of (accepted minus refused), the oldest due job older
  than 30 s and rising, or the generator late or below 98 percent of target.
- **Flow latency definitions** (`scripts/bench/flows/metrics.sql`):
  - `trigger_to_enrollment`: `enrollments.created_at` (transaction start of the `flow.enroll` job)
    minus `events.received_at` of the trigger event.
  - `end_to_end_minus_wait`: `enrollments.ended_at` minus the event's `received_at`, minus the 30 s
    wait, for completed enrollments.
  - `wake_lateness`: `started_at` of the step after the wait minus the wake instant (the `run_at` of
    the wake job, sampled while queued; when missed, the wait node's `started_at` plus 30 s). All
    129,292 wake times in `soak-150` were captured.
- **Passes.** Pass 1 (`flows/`) and pass 2 (`flows-run2/flows/`) each ran on a freshly migrated
  database. Pass 2 repeats `bg1000` as `bg1000-repeat` to check repeatability.
- **Pass 3 (worker concurrency).** `flows-conc16/flows/` and `flows-conc32/flows/`, each on a fresh
  database, run with `BENCH_WORKER_CONCURRENCY` 16 or 32 (`ZIRV_FLOW_WORKER__CONCURRENCY` in each
  `stack.txt`) and `BENCH_PG_MAX_CONNECTIONS=300` (default 100; `pg.max_connections=300` in each
  `stack.txt`). At concurrency 32 the worker's pools can reach 10 + 3 x (32 + 8 + 2) + 1 = 137
  connections by `docs/operations/sizing.md`, which is why the PostgreSQL limit was raised. The serve
  pool was 32. Each run has `warmup`, `bg1000` and a ramp at 1,000 events/s background (120 s steps):
  c16 100 to 800 by 100, c32 100 to 1,000 by 100. The c32 run had diagnostic logging on, set at
  23:31:13Z: `log_autovacuum_min_duration=0`, `log_lock_waits=on`, `log_min_duration_statement=2000`;
  the c16 run did not. Everything else was as in passes 1 and 2.
- **CPU and memory** come from `docker stats` samples (`*-docker-stats.csv`); the window and step
  JSON files carry the averages and peaks.

## 3. Ingest capacity

The accepted ceiling is the highest held step. Peak CPU is in percent of one core. No step had a 5xx
or 429.

### 3.1 Track events (one event per request)

Source: `L/track1-steps.jsonl` (pool 10), `serve-pool-32/L/track1-steps.jsonl` (pool 32).

| Pool | Rate/s | Held | API p99 from schedule (ms) | Applied p50 / p95 / p99 (ms) | Peak CPU: PostgreSQL / serve / worker | Note |
|---|---|---|---|---|---|---|
| 10 | 2,250 | yes | 17.4 | 10.9 / 20.3 / 25.1 | 627 / 146 / 164 | |
| 10 | 2,500 | yes | 21.1 | 11.3 / 18.5 / 21.3 | 584 / 165 / 160 | |
| 10 | 2,750 | yes | 68.8 | 12.5 / 20.4 / 23.2 | 610 / 181 / 150 | |
| 10 | 3,000 | no | 9,765 | 12.9 / 20.6 / 23.5 | 611 / 260 / 146 | late dispatch 119,847; achieved 2,690/s |
| 32 | 2,750 | yes | 19.8 | 12.7 / 22.1 / 27.8 | 645 / 189 / 157 | |
| 32 | 3,000 | yes | 19.4 | 13.3 / 22.3 / 27.8 | 629 / 206 / 149 | 7 transport errors |
| 32 | 3,250 | yes | 22.6 | 14.9 / 25.7 / 33.8 | 653 / 225 / 142 | 9 transport errors |
| 32 | 3,500 | yes | 44.4 | 16.9 / 30.4 / 40.0 | 677 / 248 / 130 | |
| 32 | 3,750 | yes | 36.4 | 18.5 / 33.3 / 41.7 | 695 / 270 / 120 | |
| 32 | 4,000 | yes | 27.8 | 21.2 / 38.2 / 47.2 | 703 / 278 / 110 | |
| 32 | 4,250 | yes | 53.9 | 24.5 / 44.0 / 52.5 | 738 / 287 / 103 | |
| 32 | 4,500 | no | 12,179 | 23.6 / 41.8 / 50.3 | 726 / 444 / 103 | late dispatch 219,861; achieved 3,910/s |

Pool 10 ceiling 2,750/s; pool 32 ceiling 4,250/s (55 percent higher). At both pools the first failing
step is the generator and the API falling behind (API p99 above 9 s), not applied latency. PostgreSQL
peaks at 6.1 to 7.4 cores at the ceilings. The inbox was empty at the end of every track step
(`inbox_age_end_s` 0).

### 3.2 Identify (one item per request)

Source: `L/identify1-steps.jsonl`, `serve-pool-32/L/identify1-steps.jsonl`.

| Pool | Rate/s | Held | API p99 from schedule (ms) | Applied p50 / p95 / p99 (ms) | Peak CPU: PostgreSQL / serve / worker | Note |
|---|---|---|---|---|---|---|
| 10 | 1,250 | yes | 12.2 | 4.2 / 8.6 / 13.7 | 395 / 71 / 138 | |
| 10 | 1,500 | yes | 9.8 | 7.4 / 16.2 / 20.0 | 505 / 89 / 168 | |
| 10 | 1,750 | yes | 30.3 | 8.5 / 18.7 / 22.8 | 522 / 108 / 170 | |
| 10 | 2,000 | yes | 17.1 | 9.8 / 23.1 / 28.9 | 548 / 126 / 166 | 4 transport errors |
| 10 | 2,250 | yes | 16.7 | 12.4 / 30.2 / 38.1 | 587 / 144 / 161 | |
| 10 | 2,500 | yes | 19.1 | 14.8 / 38.6 / 47.9 | 614 / 163 / 159 | |
| 10 | 2,750 | no | 187 | 17.3 / 48.1 / 61.3 | 646 / 198 / 155 | failed on API p99 |
| 32 | 2,000 | yes | 9.8 | 9.3 / 20.8 / 28.4 | 566 / 128 / 172 | |
| 32 | 2,250 | yes | 9.1 | 10.2 / 26.0 / 46.3 | 570 / 140 / 167 | |
| 32 | 2,500 | yes | 9.9 | 12.2 / 32.0 / 45.1 | 595 / 164 / 163 | |
| 32 | 2,750 | yes | 11.1 | 16.4 / 47.9 / 73.5 | 624 / 173 / 161 | |
| 32 | 3,000 | yes | 12.9 | 26.0 / 103.6 / 151.0 | 663 / 203 / 159 | applied p95 over 100 ms |
| 32 | 3,250 | yes | 15.0 | 56.5 / 190.9 / 241.0 | 665 / 226 / 153 | |
| 32 | 3,500 | yes | 18.6 | 383.0 / 813.5 / 938.9 | 682 / 259 / 137 | inbox age at end 1.0 s |
| 32 | 3,750 | yes | 69.2 | 341.6 / 1,049.7 / 1,123.7 | 750 / 257 / 130 | inbox age at end 1.1 s |
| 32 | 4,000 | yes | 29.8 | 502.4 / 1,539.1 / 1,657.4 | 695 / 282 / 132 | inbox age at end 1.7 s |
| 32 | 4,250 | yes | 45.2 | 664.1 / 1,943.9 / 2,037.2 | 704 / 290 / 155 | inbox age at end 2.0 s |
| 32 | 4,500 | yes | 49.7 | 866.1 / 2,354.7 / 2,452.2 | 721 / 302 / 119 | inbox age at end 2.6 s |
| 32 | 4,750 | no | 334.6 | 677.4 / 2,377.6 / 2,466.2 | 718 / 348 / 123 | failed on API p99 |

At pool 32 the API accepts up to 4,500 identify items/s, but applied latency is not kept down from
3,000/s: applied p50 rises from 26 ms (3,000/s) to 866 ms (4,500/s) and the inbox is not empty at the
end of the step (1.0 to 2.6 s of age). The step criteria (inbox age under 5 s and not growing) did not
catch this. The highest rate that keeps applied p95 under 100 ms is 2,750/s, so that is the
comfortable rate for identify. At pool 10 the ramp failed on API p99 at 2,750/s with applied p95 48.1
ms.

### 3.3 Batch (100 items per request, items/s)

Source: `L/batch1-steps.jsonl`, `serve-pool-32/L/batch1-steps.jsonl`. Applied latency is that of the
track-kind events the batches carry.

| Pool | Items/s | Held | API p99 from schedule (ms) | Applied p50 / p95 / p99 (ms) | Inbox age at end (s) | Peak CPU: PostgreSQL / serve / worker | Note |
|---|---|---|---|---|---|---|---|
| 10 | 2,000 | yes | 29.7 | 18.0 / 41.7 / 70.1 | 0 | 640 / 4 / 68 | |
| 10 | 4,000 | yes | 27.3 | 31.2 / 126.9 / 236.2 | 0 | 670 / 8 / 168 | |
| 10 | 6,000 | yes | 25.1 | 16.7 / 48.5 / 63.9 | 0 | 451 / 10 / 212 | |
| 10 | 8,000 | yes | 26.8 | 22.2 / 1,370.0 / 1,675.2 | 1.9 | 480 / 15 / 218 | |
| 10 | 10,000 | no | 32.9 | 15,440.5 / 19,848.8 / 20,806.9 | 16.1 | 731 / 19 / 226 | inbox age at or above 5 s |
| 32 | 2,000 | yes | 21.8 | 14.3 / 31.0 / 39.3 | 0 | 435 / 4 / 86 | |
| 32 | 4,000 | yes | 22.9 | 13.4 / 42.8 / 56.2 | 0 | 604 / 6 / 162 | |
| 32 | 6,000 | yes | 22.8 | 15.3 / 39.6 / 55.4 | 0 | 463 / 10 / 216 | |
| 32 | 8,000 | yes | 24.5 | 19.4 / 309.5 / 578.4 | 0.6 | 506 / 15 / 222 | |
| 32 | 10,000 | no | 23.7 | 28.7 / 3,541.6 / 4,518.5 | 4.2 | 499 / 17 / 219 | inbox age growing |

API p99 stays under 33 ms in every batch step: the limit is the inbox and applied latency, not the API.
The serve database pool made no difference to the accepted ceiling (8,000 in both). Applied p95 is over
100 ms at 8,000 items/s (1,370 ms at pool 10, 309.5 ms at pool 32), so the comfortable rate is 6,000
items/s. The pool 10 step at 4,000 has applied p95 126.9 ms while the 6,000 step has 48.5 ms; the data
does not say why (section 6).

### 3.4 Cold start and strict runs

- **Cold start** (`L/cold-start/`): the ramp started at 1,000/s on a fresh database with no warm-up
  and no `ANALYZE`. Track step 1,000/s: API p99 from schedule 2,110 ms, failed. Identify step
  1,000/s: 9,700 ms, failed. The recorded ceiling is 0 for both. This is why `tier.sh` warms up and
  runs `ANALYZE` before step 1.
- **Strict** (`L/strict/`, pool 10, zero transport-error tolerance): track held to 2,000/s (the 2,250/s
  step failed on API p99 2,837 ms); identify held to 1,000/s (the 1,250/s step failed on 2 transport
  errors). With the 1e-4 tolerance the pool 10 ramps held to 2,750 and 2,500. The strict runs are
  separate runs, so the difference is not only the tolerance (section 6).

## 4. Flow execution

All figures are from the window and step JSON files in `flows/` (pass 1) and `flows-run2/flows/`
(pass 2). Latencies are in seconds.

### 4.1 Capacity ramps

Each step is 120 s of trigger events at the stated rate plus the background track load. "Held" is the
`flows.sh` criterion (section 2). Enrollments/s is enrollments created inside the 120 s generator
window divided by 120. "Oldest due" is the peak age of the oldest due job in the 5 s queue samples.
CPU columns are cores (average / peak) during the generators. "e2e-w" is end-to-end minus the wait;
"wake" is wake lateness.

**No background load** (pass 2, `flows-run2/flows/ramp-0.json`; last held step 400):

| Triggers/s | Held | Enrollments/s | trig-to-enroll p50 / p95 / p99 | e2e-w p95 | wake p99 | Oldest due (s) | PostgreSQL | Worker |
|---|---|---|---|---|---|---|---|---|
| 100 | yes | 97.5 | 0.007 / 0.013 / 0.017 | 0.792 | 0.903 | 0.2 | 1.24 / 1.70 | 0.57 / 0.72 |
| 150 | yes | 144.2 | 0.008 / 0.013 / 0.019 | 0.764 | 0.880 | 0.8 | 1.78 / 2.11 | 0.85 / 1.12 |
| 200 | yes | 190.2 | 0.009 / 0.018 / 0.035 | 0.697 | 0.828 | 0.7 | 2.39 / 3.44 | 1.17 / 1.45 |
| 250 | yes | 232.0 | 0.265 / 12.6 / 14.0 | 20.5 | 13.8 | 13.0 | 3.83 / 5.56 | 1.25 / 1.66 |
| 300 | yes | 278.2 | 0.024 / 0.102 / 0.147 | 0.615 | 0.814 | 0.4 | 3.31 / 4.28 | 1.58 / 1.75 |
| 350 | yes | 315.5 | 1.17 / 1.89 / 2.07 | 4.37 | 1.95 | 1.8 | 3.63 / 4.92 | 1.71 / 1.86 |
| 400 | yes | 330.1 | 5.32 / 9.22 / 9.41 | 21.5 | 9.32 | 9.3 | 3.91 / 4.98 | 1.82 / 1.91 |
| 450 | no | 264.4 | 25.9 / 45.1 / 46.2 | 84.6 | 45.4 | 45.8 | 5.82 / 6.62 | 1.20 / 1.96 |

Step 250 is anomalous (section 6.1). Step 450 failed on oldest due job older than 30 s and rising.

**Background 1,000 events/s**, both passes in one table, ordered by trigger rate. Pass 1:
`flows/ramp-1000.json` (steps 100, 300, 500; last held 300). Pass 2: `flows-run2/flows/ramp-1000.json`
(steps 150, 200, 250; no step failed; the ramp ended at 250).

| Triggers/s | Pass | Held | Enrollments/s | trig-to-enroll p50 / p95 / p99 | e2e-w p95 | wake p99 | Oldest due (s) | PostgreSQL | Worker |
|---|---|---|---|---|---|---|---|---|---|
| 100 | 1 | yes | 97.5 | 0.013 / 0.022 / 0.031 | 0.652 | 0.871 | 0.9 | 4.50 / 5.47 | 1.96 / 2.06 |
| 150 | 2 | yes | 144.2 | 0.017 / 0.041 / 0.065 | 0.783 | 0.874 | 0.7 | 4.97 / 6.09 | 2.14 / 2.29 |
| 200 | 2 | yes | 189.9 | 0.031 / 0.137 / 0.221 | 0.792 | 0.828 | 0.7 | 5.30 / 5.99 | 2.19 / 2.32 |
| 250 | 2 | yes | 211.4 | 7.91 / 12.1 / 13.4 | 28.5 | 13.2 | 11.9 | 5.76 / 6.56 | 1.81 / 2.32 |
| 300 | 1 | yes | 229.1 | 13.6 / 19.3 / 19.8 | 47.4 | 19.6 | 19.3 | 5.54 / 6.74 | 1.64 / 2.39 |
| 500 | 1 | no | 296.6 | 25.1 / 34.3 / 35.0 | 76.8 | 34.8 | 34.7 | 5.77 / 7.03 | 1.67 / 2.39 |

Pass 1 has no step between 100 and 300 and pass 2 none above 250, so the knee lies between 200 and 250
triggers/s. Step 500 failed on oldest due job older than 30 s and rising. The practical limit
(trigger-to-enrollment p95 under 1 s) is 200 triggers/s.

**Background 2,000 events/s** (pass 2, `flows-run2/flows/ramp-2000.json`; last held step 350):

| Triggers/s | Held | Enrollments/s | trig-to-enroll p50 / p95 / p99 | e2e-w p95 | wake p99 | Oldest due (s) | PostgreSQL | Worker |
|---|---|---|---|---|---|---|---|---|
| 100 | yes | 97.4 | 0.023 / 0.748 / 1.66 | 1.52 | 1.29 | 0.9 | 6.22 / 6.79 | 1.81 / 2.10 |
| 150 | yes | 144.1 | 0.034 / 0.143 / 0.274 | 0.799 | 0.884 | 0.9 | 6.13 / 6.73 | 2.04 / 2.15 |
| 200 | yes | 187.2 | 0.334 / 1.78 / 2.14 | 4.02 | 1.97 | 1.9 | 6.01 / 6.52 | 2.15 / 2.35 |
| 250 | yes | 210.0 | 6.49 / 11.6 / 12.1 | 25.9 | 11.9 | 12.1 | 6.39 / 6.82 | 2.05 / 2.17 |
| 300 | yes | 231.3 | 11.6 / 19.1 / 19.6 | 40.0 | 19.3 | 19.6 | 6.47 / 6.78 | 2.05 / 2.17 |
| 350 | yes | 242.6 | 16.9 / 27.3 / 28.3 | 53.6 | 27.8 | 28.3 | 6.53 / 6.81 | 2.00 / 2.16 |
| 400 | no | 246.0 | 26.8 / 34.9 / 35.5 | 79.6 | 34.5 | 35.1 | 6.32 / 6.79 | 1.36 / 2.14 |

Step 400 failed on oldest due job older than 30 s and rising.

Limits from the three ramps:

| Background events/s | Last held step (60 s criterion) | Practical limit (p95 under 1 s) | Flow entries per day at the practical limit |
|---|---|---|---|
| 0 | 400 triggers/s | 300 (200 counting the stalled step 250) | 25.9 M (17.3 M) |
| 1,000 | 300 (pass 1); pass 2 reached 250 without failing | 200 | 17.3 M |
| 2,000 | 350 | 150 | 13.0 M |

Daily figures are the rate times 86,400 s.

Past the knee the queue backs up: the oldest due job's peak age grows with the step (0.9, 19.3 and 34.7
s at 100, 300 and 500 triggers/s with 1,000 events/s of background) while the worker's average CPU
stays between 1.6 and 2.2 cores. Background ingest lowers the knee: the practical limit falls from
300 (none) to 200 (1,000/s) to 150 (2,000/s) triggers/s.

### 4.2 Windows at fixed load

In the two soaks (pass 2 `soak60-100`, pass 1 `soak-150`) the background generator achieved 999.99 and
999.98 events/s and the trigger generator 100.0 and 150.0 triggers/s, with no late dispatch, no 429
and no 5xx. API p99 from schedule: background 10.3 ms and 10.2 ms, triggers 17.9 ms and 18.8 ms.
Ingest applied latency (track events): p95 16.6 and 16.4 ms, p99 22.5 and 21.9 ms. The background
generator had one transport error in each soak (tolerance 1e-4).

| Window | Refused `already_active` | Failed | Dead jobs | Max trig-to-enroll (s) | Max e2e-w (s) | Max wake (s) | Oldest due, peak (s) |
|---|---|---|---|---|---|---|---|
| `soak60-100` | 10,441 | 0 | 0 | 0.376 | 1.091 | 0.986 | 0.5 |
| `soak-150` | 5,706 | 0 | 0 | 0.218 | 1.147 | 1.002 | 0.9 |
| `idle` | 196 | 0 | 0 | 0.114 | 1.025 | 0.971 | 0.9 |
| `bg1000` | 217 | 0 | 0 | 0.137 | 1.039 | 0.993 | 0.9 |
| `bg1000-repeat` | 220 | 0 | 0 | 0.060 | 1.033 | 0.972 | 0.8 |
| `bg2000` | 215 | 0 | 0 | 0.116 | 1.059 | 0.977 | 0.8 |

Enrollment and step rates: `soak60-100` 97.10 enrollments/s and 483.87 step executions/s;
`soak-150` 143.66 and 708.72.

CPU during the generators (average / peak cores): `soak60-100` PostgreSQL 4.82 / 6.68, worker
2.00 / 2.20, serve 0.56 average; `soak-150` PostgreSQL 5.09 / 5.95, worker 2.19 / 2.30, serve 0.58
average. Peak memory: `soak60-100` PostgreSQL 8,915 MiB, worker 145 MiB, serve 137 MiB; `soak-150`
PostgreSQL 5,846 MiB, worker 52 MiB, serve 59 MiB. Per-5 s queue samples are in
`soak60-100-samples.ndjson` and `soak-150-samples.ndjson`.

Repeatability: `bg1000` (pass 1) and `bg1000-repeat` (pass 2, a fresh database) give trigger-to-enrollment
p50 / p95 / p99 of 0.013 / 0.022 / 0.031 s and 0.013 / 0.023 / 0.031 s.

### 4.3 Wake lateness at light load

Wake lateness has a tail near the worker's 1 s `poll_interval` (`crates/zirv-flow-jobs/src/worker.rs:143`)
whose size depends on the window length, not on the load:

| Window | Rate, triggers/s | Generator time | Share of wakes due after the generators stop | Wake lateness p95 / p99 (s) |
|---|---|---|---|---|
| `soak60-100` | 100 | 3,600 s | 0.8% | 0.015 / 0.029 |
| `soak-150` | 150 | 900 s | 3.3% | 0.025 / 0.611 |
| `bg1000` | 50 | 300 s | 10% | 0.481 / 0.829 |
| `flows/ramp-1000-100` | 100 | 120 s | 25% | 0.593 / 0.871 |

The share is the flow's 30 s wait divided by the generator time. The percentile at which lateness jumps
to about 0.5 to 0.9 s follows that share. The hypothesis, not measured here: while triggers arrive, each
new job wakes the worker through LISTEN/NOTIFY and due wakes are claimed at once; the wakes that come due
after the generators stop find an idle worker and wait for its next 1 s poll. If so, in steady production
traffic wake lateness is the soak's (29 ms at p99), and an idle system adds up to 1 s. Pass 3 does not fit
the pattern exactly: at concurrency 16 and 32 the ramp steps have lower wake p99 (for example 0.210 s at
c16 step 500 and 0.114 s at c32 step 600, both 120 s steps); the data does not say why.

## 5. Worker concurrency

Pass 3 repeats the 1,000 events/s ramp of section 4.1 with `worker.concurrency` 16 and 32 instead of 4
(method: section 2). Sources: `flows-conc16/flows/` and `flows-conc32/flows/` (`ramp-1000.json`, the
per-step `ramp-1000-<rate>.json`, `bg1000.json`). The c4 columns are passes 1 and 2 of section 4.1.

### 5.1 Same trigger rates at concurrency 4, 16 and 32

Background 1,000 events/s. Enrollments/s and trigger-to-enrollment p95 (s). "not run" marks steps that
ramp did not run; "none" marks the c32 step 1,000, which has no result (section 6, item 11).

| Triggers/s | c4: enrollments/s (pass) | c4: p95 | c16: enrollments/s | c16: p95 | c32: enrollments/s | c32: p95 |
|---|---|---|---|---|---|---|
| 100 | 97.5 (1) | 0.022 | 97.5 | 0.024 | 97.5 | 0.024 |
| 200 | 189.9 (2) | 0.137 | 189.9 | 0.032 | 189.9 | 0.034 |
| 300 | 229.1 (1) | 19.3 | 277.8 | 0.048 | 277.8 | 0.079 |
| 400 | not run | not run | 354.4 | 14.8 | 361.8 | 0.069 |
| 500 | 296.6 (1) | 34.3 | 442.0 | 0.137 | 442.0 | 0.086 |
| 600 | not run | not run | 517.5 | 0.712 | 518.0 | 0.118 |
| 700 | not run | not run | 563.5 | 4.96 | 550.9 | 31.7 |
| 800 | not run | not run | 406.4 (failed) | 57.5 | 659.3 | 0.744 |
| 900 | not run | not run | not run | not run | 710.3 | 2.49 |
| 1,000 | not run | not run | not run | not run | none | none |

The c4 steps at 300 and 500 (pass 1) had already passed the knee (p95 19.3 s and 34.3 s); c4 at 500
failed the 60 s criterion. c16 failed at 800; c32 held 100 to 900.

### 5.2 Limits

| Concurrency | Highest step with p95 under 1 s | Highest step below which every step is under 1 s | Practical limit as flow entries per day (highest step under 1 s) | Highest enrollments/s at a held step |
|---|---|---|---|---|
| 4 (passes 1 and 2) | 200 | 200 | 17.3 M | 229.1 at 300 (pass 1); 296.6 at 500, which failed |
| 16 | 600 (p95 0.712 s) | 300 | 51.8 M | 563.5 at 700 |
| 32 | 800 (p95 0.744 s) | 600 | 69.1 M | 710.3 at 900 |

The two columns differ because of transient stalls in held steps: c16 step 400 (p95 14.8 s, between
clean steps 300 at 0.048 s and 500 at 0.137 s) and c32 step 700 (p95 31.7 s, between 600 at 0.118 s and
800 at 0.744 s). For c4 no step below the limit stalled in passes 1 and 2 (the ramp-0 stall at step 250,
section 6.1, is a different ramp). The c32 ceiling is not reached under the 60 s criterion: the step
at 900 held, and the step at 1,000 was lost to a PostgreSQL crash (section 6, item 11). Both `ramp-1000.json`
files carry a `verdict`; the c32 one is assembled from the per-step records and says so.

### 5.3 Concurrency 16

`flows-conc16/flows/ramp-1000.json` (last held step 700; failed at 800 on
`generator_achieved_below_98pct`). CPU is cores (average / peak) during the generators.

| Triggers/s | Held | Enrollments/s | trig-to-enroll p50 / p95 / p99 | e2e-w p95 | wake p99 | Oldest due (s) | Ingest applied p95 (ms) | PostgreSQL | Worker |
|---|---|---|---|---|---|---|---|---|---|
| 100 | yes | 97.5 | 0.015 / 0.024 / 0.033 | 0.777 | 0.859 | 0.9 | 16.0 | 4.74 / 5.74 | 1.99 / 2.14 |
| 200 | yes | 189.9 | 0.018 / 0.032 / 0.046 | 0.536 | 0.756 | 0.6 | 19.4 | 5.65 / 6.41 | 2.27 / 2.48 |
| 300 | yes | 277.8 | 0.024 / 0.048 / 0.070 | 0.294 | 0.500 | 0.2 | 27.0 | 6.15 / 7.01 | 2.45 / 2.65 |
| 400 | yes | 354.4 | 0.995 / 14.8 / 16.4 | 22.1 | 13.6 | 12.4 | 5,222.3 | 6.42 / 7.52 | 1.76 / 2.92 |
| 500 | yes | 442.0 | 0.041 / 0.137 / 0.230 | 0.281 | 0.210 | 0.1 | 39.7 | 6.82 / 7.54 | 2.50 / 2.79 |
| 600 | yes | 517.5 | 0.198 / 0.712 / 0.826 | 1.46 | 0.697 | 0.5 | 43.1 | 7.09 / 7.68 | 2.52 / 2.85 |
| 700 | yes | 563.5 | 2.89 / 4.96 / 5.08 | 11.2 | 4.93 | 4.7 | 45.3 | 7.29 / 7.67 | 2.69 / 3.18 |
| 800 | no | 406.4 | 40.7 / 57.5 / 58.3 | 58.8 | 8.06 | 10.0 | 56,073.5 | 5.42 / 7.54 | 0.88 / 3.46 |

### 5.4 Concurrency 32

`flows-conc32/flows/ramp-1000.json` (steps 100 to 900, all held; step 1,000 has no result).

| Triggers/s | Held | Enrollments/s | trig-to-enroll p50 / p95 / p99 | e2e-w p95 | wake p99 | Oldest due (s) | Ingest applied p95 (ms) | PostgreSQL | Worker |
|---|---|---|---|---|---|---|---|---|---|
| 100 | yes | 97.5 | 0.014 / 0.024 / 0.033 | 0.767 | 0.845 | 0.7 | 15.8 | 4.86 / 5.58 | 2.08 / 2.19 |
| 200 | yes | 189.9 | 0.019 / 0.034 / 0.049 | 0.665 | 0.805 | 0.7 | 20.2 | 5.61 / 6.25 | 2.38 / 2.56 |
| 300 | yes | 277.8 | 0.030 / 0.079 / 0.120 | 0.285 | 0.473 | 0.1 | 40.4 | 6.63 / 7.39 | 2.36 / 2.80 |
| 400 | yes | 361.8 | 0.033 / 0.069 / 0.102 | 0.188 | 0.370 | 0.1 | 39.0 | 6.54 / 7.29 | 2.48 / 2.80 |
| 500 | yes | 442.0 | 0.041 / 0.086 / 0.132 | 0.219 | 0.198 | 0.1 | 49.5 | 6.76 / 7.43 | 2.43 / 2.68 |
| 600 | yes | 518.0 | 0.052 / 0.118 / 0.203 | 0.296 | 0.114 | 0.1 | 67.3 | 7.02 / 7.77 | 2.54 / 3.00 |
| 700 | yes | 550.9 | 5.84 / 31.7 / 33.9 | 33.1 | 5.52 | 5.2 | 30,164.1 | 6.81 / 7.92 | 1.89 / 3.56 |
| 800 | yes | 659.3 | 0.094 / 0.744 / 0.916 | 1.48 | 0.705 | 0.3 | 113.3 | 7.54 / 7.91 | 2.82 / 3.41 |
| 900 | yes | 710.3 | 1.82 / 2.49 / 2.69 | 6.39 | 2.39 | 1.8 | 213.1 | 7.62 / 7.96 | 2.68 / 3.29 |

### 5.5 What limits flows once concurrency is raised

PostgreSQL averages 6.4 to 7.6 cores (peak 7.3 to 8.0) from step 400 at c16 and from step 300 at c32,
while the worker stays at 1.8 to 2.8 cores on average (peak 2.7 to 3.6) in the same steps. With 16 or 32 slots the worker
no longer limits; PostgreSQL's cpuset (8 cores) is at its edge at the knee. The data shows PostgreSQL CPU
near saturation at the steps where p95 rises; it does not by itself show that CPU is the cause of the
stalls (section 6).

### 5.6 Latency at 50 triggers/s, background 1,000 events/s

`bg1000` (300 s) at concurrency 4 (pass 1, `flows/bg1000.json`), 16 (`flows-conc16/flows/bg1000.json`)
and 32 (`flows-conc32/flows/bg1000.json`); 14,783 enrollments in each, 0 failed, 0 dead jobs. Seconds.

| Concurrency | trig-to-enroll p50 / p95 / p99 | e2e-w p50 / p95 / p99 | wake lateness p50 / p95 / p99 | Worker average cores | PostgreSQL average cores |
|---|---|---|---|---|---|
| 4 | 0.013 / 0.022 / 0.031 | 0.031 / 0.518 / 0.886 | 0.007 / 0.481 / 0.829 | 1.74 | 4.07 |
| 16 | 0.014 / 0.025 / 0.033 | 0.034 / 0.506 / 0.856 | 0.007 / 0.463 / 0.800 | 1.77 | 4.42 |
| 32 | 0.014 / 0.025 / 0.032 | 0.034 / 0.378 / 0.766 | 0.007 / 0.340 / 0.699 | 1.74 | 4.41 |

Trigger-to-enrollment is unchanged by concurrency at this load. End-to-end minus wait and wake lateness
are slightly lower at c16 and c32 in the upper percentiles (p99 wake 0.829, 0.800, 0.699 s), and all
stay under the 1 s poll interval of section 4.3. The soak of section 4 was not repeated at concurrency 16
or 32.

## 6. Anomalies and limits of this run

1. **Unexplained backlog stall in `ramp-0` step 250.** The step's trigger-to-enrollment p95 is 12.6 s
   and the oldest due job reached 13.0 s, while step 300 (higher load) is clean (p95 0.102 s, oldest
   due 0.4 s). In `flows-run2/flows/ramp-0-250-samples.ndjson` the due queue is near zero until about
   27 s into the step, then grows from 228 jobs (33 s) to 6,350 jobs (60 s) and drains to 1 by 82 s,
   about 45 s in all. No error was found in the PostgreSQL log, and a timed checkpoint was in progress.
   The same kind of stall appeared later in the pass 3 runs (item 9). It recurred at this step on
   1.0.39, so the cause item 9 gives does not account for it ([the 1.0.39
   rerun](tier-l-1.0.39-rerun.md), section 3.2). This step is why section 1 gives the practical limit at no background load
   as 300, with 200 if this step is counted.
2. **`ramp-2000` step 100 is slower than step 150.** Trigger-to-enrollment p95 0.748 s (p99 1.66 s)
   against 0.143 s (0.274 s) at step 150; end-to-end minus wait p95 1.52 against 0.799 s. Step 100 is
   the first step under the heavier ingest load of that ramp. The data does not give a cause.
3. **Throughput fell at the failing steps of `ramp-0`:** 330 enrollments/s at 400 triggers/s and 264 at
   450, while the worker's average CPU fell from 1.82 to 1.20 cores. The other ramps did not fall at
   their failing step (`ramp-1000` pass 1: 229 at 300, 297 at 500; `ramp-2000`: 243 at 350, 246 at 400).
4. **Not recorded:** `ramp-2000` step 150 has no ingest applied-latency value (`null` in
   `ramp-2000-150.json`); the `idle` and `warmup` windows have no background generator.
5. **Batch ramps use the fixed steps of `tier.sh`** (2,000 to 24,000 by 2,000), so the batch ceiling is
   only known to lie between 8,000 and 10,000 items/s. The pool 10 batch step at 4,000 items/s has
   applied p95 126.9 ms against 48.5 ms at 6,000; this is not explained.
6. **Transport-error tolerance and cold start.** The ramps allow transport errors up to 1e-4 of accepted
   requests (observed: 4 at pool 10 identify 2,000/s; 7 and 9 at pool 32 track 3,000 and 3,250/s; one
   in each soak). The strict runs (`L/strict/`) are separate runs, not the same steps re-scored: they
   held to 2,000 (track) and 1,000 (identify). The cold start (`L/cold-start/`) shows a fresh database
   fails at 1,000/s without a warm-up and `ANALYZE`. A first attempt at pool 32 warmed up at 2,750/s on
   a fresh database (`serve-pool-32/L-warmup-at-2750/`): the warm-up got 82,559 of 215,072 requests
   accepted (132,513 transport errors), and the 2,750/s step accepted 168,126 requests at 1,867/s with
   79,374 transport errors and 113,709 late dispatches; the recorded ceiling is 0. That run is confounded
   by the warm-up rate and is not used. The pool 32 track ramp warmed up at 1,000/s instead.
7. **Scope.** One host, one image, one run per configuration except `bg1000` (two). Each pool 32 ramp
   ran once. Flows used 100,000 people; the flow is one small flow (branch, trait or list, wait,
   increment, exit), so other flows will cost more or less per enrollment. Refused triggers
   (`already_active`) are 1.3 to 3.9 percent of triggers in the held windows and steps at 50 to 150
   triggers/s, and 11 to 20 percent at the highest steps (ramp-0 step 400: 11 percent; failing steps:
   17 to 20 percent).
8. **Ingest ramps at pool 10 start at different rates** (track 2,250, identify 1,250) from the pool 32
   ramps (track 2,750, identify 2,000), so the lowest steps of the tables differ; the ceilings are not
   affected.

9. **Transient stalls in held steps recur, at different concurrencies; partly explained after the run.** All
   three are steps that held under the 60 s criterion but have a p95 far above their neighbours:

   | Run | Step | Step start (UTC) | trig-to-enroll p95 (s) | Neighbours | Ingest applied p95 (ms) |
   |---|---|---|---|---|---|
   | c4 `ramp-0` | 250 | 2026-11-25 20:41:41 | 12.6 | 200: 0.018; 300: 0.102 | 49.2 |
   | c16 `ramp-1000` | 400 | 2026-11-25 23:16:44 | 14.8 | 300: 0.048; 500: 0.137 | 5,222.3 |
   | c32 `ramp-1000` | 700 | 2026-11-25 23:57:45 | 31.7 | 600: 0.118; 800: 0.744 | 30,164.1 |

   Sources: the per-step files `ramp-0-250.json`, `ramp-1000-400.json` (c16), `ramp-1000-700.json`
   (c32) and the `since` field in each.

   In c32 step 700, all 202 worker slow-statement warnings of the whole run (threshold 1 s,
   `flows-conc32/flows/worker-slow-statements.txt`) fall between 23:58:13 and 23:58:37, inside that
   step. The kinds: the re-entry check on `enrollments` (80), the goal/exit lookup joining `enrollments`
   and `flow_versions` (62), the job claim `UPDATE jobs` (52), the `people` row lock (6) and the enroll
   advisory lock (2). Each took 1.00 to 1.56 s, although these statements return a few rows. The
   PostgreSQL log (`postgres-log-excerpts.txt`) also records lock waits of about 1.1 to 1.2 s on a
   `people` tuple and on the enroll advisory lock in the same period. A WAL-triggered checkpoint
   started at 23:58:22. The step's ingest inbox had started growing at about 23:57:50 (3,533 rows at
   23:57:51; peak 54,126 rows with the oldest item 33.0 s old, in `ramp-1000-700-samples.ndjson` and
   `ramp-1000-700.json`), before the first slow statement. The re-entry check's plan is an index-only
   scan on `enrollments_in_flight_idx`, checked with EXPLAIN ANALYZE on the live database after the
   step drained (`flows-conc32/flows/explain-reentry-check.txt`). So during the stall the statements
   ran slowly with a correct plan.

   The checkpoint start does not line up with the other stalls: c4 `ramp-0` step 250 began at
   20:41:41Z and its last checkpoint had started at 20:38:07 (from the run's log, not in the data
   directory); c16 step 400 began at 23:16:44Z, during a checkpoint that ran from 23:12:20 to 23:19:36
   (`flows-conc16/flows/postgres.log`).

   **A cause, found after the run.** Each step ends with no enrollment in flight, because the runner
   waits for that. When autoanalyze samples `enrollments` in that gap, it records `state` as 100 percent
   `completed`, and the planner then believes the in-flight partial index `enrollments_in_flight_idx` is
   empty. Under that belief the re-entry check (`flow_id = $1 AND person_id = ANY($2)`) and the goal/exit
   lookup are planned as scans of every in-flight enrollment rather than one probe per person, so the
   cost of each call grows with the in-flight count through the step, until the next autoanalyze.
   Reproduced without docker on PostgreSQL 16.16 and 18.6, with 244,000 completed and 20,000 in-flight
   enrollments analyzed while none were in flight: the re-entry check read about 20,100 buffers (9 to
   14 ms) per call and the goal/exit lookup about 20,150 (12 to 110 ms), against about 121 buffers and
   0.1 to 0.35 ms with fresh statistics. This fits the data. PostgreSQL CPU rose at constant load while
   the worker idled (c4 step 250: 2.8 to 5.6 cores in 33 s). In c32 the stall ended as autovacuum
   analyzed `enrollments` at 23:58:37 (`postgres-log-excerpts.txt`), and the inbox fell from 54,126 rows
   at 23:58:34 to 17,454 at 23:58:46. The EXPLAIN above ran after that analyze, so it saw fresh
   statistics. The checkpoints play no part.

   1.0.39 fixes it. Migration 0116 rebuilds `enrollments_in_flight_idx` person first and adds
   `enrollments_held_idx`, so the paused-flow resume walk stays bounded too. The re-entry check, the
   goal/exit lookup and the segment goal/exit lookup now run one probe per person. With the same stale
   statistics they read about 121 to 180 buffers on 16 and 18, in both plan modes. Still exposed: the
   flow-scoped in-flight counts (a flow's people-in-flow count and the summary rollup) scan the
   in-flight index in full under stale statistics, about 20,120 buffers against 316 to 527. They run
   once per request or per rollup, not once per event. Re-benchmarked in [the 1.0.39
   rerun](tier-l-1.0.39-rerun.md), section 3. With the statistics forced stale before every step,
   1.0.38 got slower step after step and failed, and 1.0.39 recovered by the next step. Single-step
   stalls of the kind in the table still occur on 1.0.39, at c4 `ramp-0` step 250 and at c16 and c32
   step 400. They end at the first autovacuum pass inside the step, so some stale-statistics effect
   remains, in a query 0116 did not change; which query is not established.
10. **Concurrency 16 collapses at step 800; it is not a plateau.** Enrollments/s fall from 563 (step 700)
    to 406 (step 800). In `ramp-1000-800.json` the trigger generator's API p99 from schedule is
    30,003 ms, the generator's request timeout, and the background generator's is 30,003 ms too: the
    requests hung and were not refused (0 429, 0 5xx; 13,237 trigger-generator and 16,548
    background-generator transport errors). The trigger generator achieved 689.6/s against 800. Ingest
    applied p99 is 57,411 ms (p95 56,074 ms), the inbox's oldest item reached 56.8 s with 101,619 rows
    at the peak, and the oldest due job peaked at 10.0 s. `flows-conc16/flows/postgres.log` has a
    `canceling statement due to statement timeout` on `SELECT EXTRACT(EPOCH FROM now() - min(wake_at))
    ... FROM enrollments WHERE state = 'waiting' AND wake_at <= now()` at 23:28:20 (step 800 began at
    23:27:50Z), then a burst of `could not receive data from client: Connection reset by peer` from
    23:28:48 to 23:29:22 (55 lines in the file in all, including a few at 22:58). The app containers'
    logs of this run were lost when the next stack replaced them, so the application side of the
    collapse (what the worker and serve logged) is not available.

    **Cause, found after the run.** serve's database pool used sqlx's default acquire timeout of 30 s.
    A request that could not get a connection waited 30 s, so the 503 the code returns on a pool
    timeout came no earlier than the generator's own 30 s timeout, and the generator recorded transport
    errors, not refusals. In 1.0.39 a request waits at most 5 s for a connection, and an ingestion
    request then gets a 503 with `Retry-After: 5`. Re-benchmarked in [the 1.0.39
    rerun](tier-l-1.0.39-rerun.md), section 4. Overload is now refused with 503, but only after up
    to about 10 s, because an ingest request waits for two connections, one after the other. The 30 s
    transport errors here and on 1.0.39 coincide with the load-generator host running out of
    ephemeral ports: Windows logged Tcpip event 4227 at 23:28:16 UTC, 26 s into this step.
11. **PostgreSQL crash in the c32 run, step 1,000.** At 2026-11-26T00:09:03Z a PostgreSQL 16.16
    backend running the benchmark's own `scripts/bench/flows/metrics.sql` died with signal 11 (a
    segmentation fault). The postmaster then ended every other session and entered recovery mode
    (`flows-conc32/flows/postgres-log-excerpts.txt`, the crash section, with the failed query; also the
    `verdict` of `ramp-1000.json`). A query of the Windows System log found no WHEA-Logger (hardware
    error) event in the 7 days before; that check is not a file here. The step's result is lost; its
    samples and docker stats are kept. This host has a documented history of
    CPU computation faults that crashed other processes; a link to this crash is possible, not shown.
    The crash happened in a benchmark query, not in a product request, so it is not evidence about the
    product. It is why the c32 ramp stops at 900 with the ceiling not reached. Investigated again after
    the run: there is no core file and no hardware event, so the cause is still not established, and no
    product change follows from it. The same query failed twice more on 1.0.39 ([the
    rerun](tier-l-1.0.39-rerun.md), section 6).
12. **Pass 3 limits.** One ramp per concurrency. The c32 run had diagnostic logging on and the c16 run
    did not, so their PostgreSQL overhead differs slightly. The step-to-step comparison with c4 uses
    passes 1 and 2, which have steps at different rates (section 5.1). The 1-hour soak was not run at
    c16 or c32.

## 7. Raw data

All paths are under `docs/benchmarks/data/tier-l-1.0.38/`.

- `L/`: ingest ramps at serve pool 10 (track1, identify1, batch1): `*-steps.jsonl` (one line per
  step), `*-ceiling.json`, `*-warmup.json`, `*-stack.txt`, `*-pg-facts.txt`, `*-docker-stats.csv`,
  `*-inbox-samples.csv`, `*-pg-connections.csv`.
- `L/strict/`: track1 and identify1 at pool 10 with zero transport-error tolerance.
- `L/cold-start/`: track1 and identify1 at pool 10 from a fresh database without warm-up or `ANALYZE`.
- `serve-pool-32/L/`: the same three ramps re-run at serve pool 32.
- `serve-pool-32/L-warmup-at-2750/`: the failed first attempt at pool 32 track (warm-up at 2,750/s on
  a fresh database); not used for results.
- `flows/`: flow pass 1 (fresh database): `warmup`, `idle`, `bg1000`, `bg2000`, `ramp-1000` (steps
  100, 300, 500), `soak-150`, plus `seed.json`, `seed-summary.json`, `flow-definition.json` and
  `stack.txt`; each window has `<label>.json`, `<label>-samples.ndjson` (5 s queue samples) and
  `<label>-docker-stats.csv`.
- `flows-run2/flows/`: flow pass 2 (fresh database): `warmup`, `bg1000-repeat`, `ramp-0` (100 to 450
  by 50), `ramp-1000` (150 to 250), `ramp-2000` (100 to 400), `soak60-100`, with the same file set as
  pass 1.
- `flows-conc16/flows/`: flow pass 3 at worker concurrency 16 (fresh database,
  `pg.max_connections=300`): `warmup`, `bg1000`, `ramp-1000` (100 to 800 by 100, per-step
  `ramp-1000-<rate>.json`, `-samples.ndjson` and `-docker-stats.csv`), `seed.json`, `seed-summary.json`,
  `flow-definition.json`, `stack.txt`, and `postgres.log` (the PostgreSQL log of the whole run). The
  app containers' logs of this run were lost when the next stack replaced them.
- `flows-conc32/flows/`: flow pass 3 at worker concurrency 32 (fresh database,
  `pg.max_connections=300`): the same file set, with `ramp-1000` covering 100 to 900 (the 1,000 step has
  only samples and docker stats, no result file, and `ramp-1000.json` was assembled from the per-step
  records), plus `postgres-log-excerpts.txt` (step 700 and the crash; diagnostic logging was on) and
  `worker-slow-statements.txt` (summary of the worker's slow-statement warnings, threshold 1 s) and
  `explain-reentry-check.txt` (the re-entry check's plan after step 700).
