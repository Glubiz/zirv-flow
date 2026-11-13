# 5x simulated production benchmark of 1.0.0

This page reports a benchmark of the published image `ghcr.io/glubiz/zirv-flow:1.0.0` on the
[simulated production stack](../operations/simulated-production.md#the-x5-profile), started from an empty
database with the x5 profile. It ran on one development machine with CPU and memory caps. It is **not
reference hardware**; the re-run on reference hardware is
[#606](https://github.com/Glubiz/zirv-flow/issues/606). For what to do with the numbers, see
[Scaling past the 5x simulated load](../operations/sizing.md#scaling-past-the-5x-simulated-load).

Labels: **M** is measured, **D** is derived from measured values. Cores are cgroup cores. The image digest
was `sha256:bb91d481fb51e0d5759bab141786c409c170f08b84441e3a492191751a2261af`, checked against the local
image before the run; the migrate container applied migrations up to 95 on the empty database.

## 1. Setup

| Item | Value |
| --- | --- |
| Load | The x5 profile: 500 k people and a daily-curve peak of 6,500 events/s in total, `sim.sh rate 3490`. About 4,400/s of the peak arrive through the ingest API (68%) and about 1,430/s through Kafka (22%) |
| Rates | "The 5x daily-curve peak" is the peak of that curve. "The daily mean" is the peak divided by 1.862 (D), about 2,360/s through the ingest API. Window W3 runs the daily-curve peak (about 4,400/s offered through the ingest API), W2 (2,500/s) is just above the daily mean, and W1 (1,000/s) is a light load |
| Database | `zf-db`: 8 CPUs, 10 GB; PostgreSQL 16.15 with the settings in [stack.txt](data/x5-1.0.0/stack.txt): `shared_buffers` 4 GB, `effective_cache_size` 8 GB, `max_connections` 350, `jit` off, `checkpoint_timeout` 1800 s, `max_wal_size` 64 GB, `wal_compression` lz4, `work_mem` 4 MB, `synchronous_commit` on |
| App | 2 `serve` (1 CPU, 1 GB each), 4 workers (1.375 CPUs, 2 GB each), 1 scheduler (0.5 CPUs, 512 MB), the profile's caps, unchanged. 16 drain shards, 4 drain slots per worker |
| Host | One Docker VM with 32 CPUs and 33.5 GB on WSL2 kernel 6.18, on an Intel Core i9-13900K. One development machine, not reference hardware |
| Windows | W1, W2, W3, P1 and P12, each about 610 s, in that fixed order, after a dry run W0. P1 and P12 are the connection-setting windows of section 8 |

Two terms in the tables:

- **Applied/s** is `zirv_flow_ingest_applied_total`: items drained from the ingest inbox, which are API ingest
  plus hooks and contacts. Kafka stream records use the sync path and are not in it.
- **Accepted/s** is `zirv_flow_ingest_items_total`. Applied exceeds accepted by a few percent at steady state
  because hook items arrive through a different endpoint, so compare applied with accepted only loosely. The
  inbox slope (applied minus arriving) is the keep-up signal.

Seeding cost (M): `sim.sh start` reached the ready gate after 248 s with 501,786 people. The backfill was
complete at 2,551 s. Sim-up time from the first start to `sim.sh down` was 118 minutes.

## 2. Verdict

- **At the peak rate, 1.0.0 did not keep up.** At W1 and W2 the inbox stayed flat (slope 0 and -11/s). At
  W3, the 5x daily-curve peak, 1.0.0 applied 4,475/s and the inbox still grew by 195/s, from 179k to 298k
  items, with the inbox age rising from 51 to 138 s (M). That is about 4% short at the peak, with no steady
  state: applied swung between 2,760 and 6,300/s in 30 s samples. The daily mean is about half this rate, and
  W2, just above the mean, kept up.
- **The first limit is PostgreSQL CPU, the 8-core cap of `zf-db`.** The database used 7.83 of 8 cores at W2 and
  7.97 at W3, throttled 6.1 and 7.7 s per second (M). The app used 3.0 to 3.2 of its 8 cores (M).
- **Inside the database the waits point at row contention.** The resolve path (`people ... FOR UPDATE`) had 4.6
  backends in Lock wait at W3 (M). See section 6.
- **P1 and P12 are inconclusive** (section 8).

## 3. Window table

M unless noted. "Offered" is the ingest generator's target, derived from its stats. "DB ms/item" and "app
ms/item" are cgroup CPU divided by applied items (D). The inbox slope is applied versus arriving, so it
includes catch-up of the backlog the window started with. Data:
[windows.csv](data/x5-1.0.0/windows.csv).

| Window | Offered/s | Accepted/s | Applied/s | Inbox slope /s | DB cores (throttled s/s) | App cores | DB ms/item (D) | App ms/item (D) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| W1 | 1,000 | 999 | 1,059 | +0 | 4.11 (0.05) | 1.65 | 3.88 | 1.56 |
| W2 | 2,500 | 2,525 | 2,650 | -11 | 7.83 (6.13) | 3.02 | 2.96 | 1.14 |
| W3 (daily-curve peak) | 4,420 | 4,456 | 4,475 | +195 (179k to 298k, age 51 to 138 s) | 7.97 (7.66) | 3.07 | 1.78 | 0.69 |
| P1 | 4,420 | 4,441 | 3,230 | +1,419 (131k to 1.00M) | 7.96 (6.64) | 2.93 | 2.46 | 0.91 |
| P12 | 4,420 | 4,398 | 4,151 | +441 (30k to 299k) | 7.94 (6.01) | 3.17 | 1.91 | 0.76 |

The DB cost per item falls as the rate rises because batching amortises: statements per applied item fell
from 18.6 at W1 to 9.8 at W2 to 5.1 at W3 (M). The windows began with different backlogs (W3 179k, P1 131k,
P12 30k), and 4,420/s is the ingest generator's target at the peak, not the whole 6,500/s (the Kafka share
does not pass through the inbox).

## 4. Per-role CPU and memory

CPU is in the window table above and in [containers.csv](data/x5-1.0.0/containers.csv), per container per
window. At W3 the app side was far from its caps (M): workers 2.3 to 2.5 cores of 5.5, `serve` 0.5 to 0.75 of 2,
total 3.0 to 3.2 of 8.

Memory is the cgroup `memory.current` at the end of each window (M); anon is the resident part:

| Role | Limit | W1 | W3 | P12 | Peak seen by `docker stats`, all windows |
| --- | --- | --- | --- | --- | --- |
| `zf-db` | 10 GiB | 10,240 MiB (anon 1,600, page cache 7,441) | 10,240 (anon 1,707, cache 6,838) | 10,240 (anon 2,027, cache 6,488) | 9.7 to 9.8 GiB |
| `zf-serve-1`, `-2` | 1 GiB | 10 MiB | 10 to 13 MiB | 9 to 10 MiB | 9 to 14 MiB |
| `zf-worker-1` to `-4` | 2 GiB each | 19 to 28 MiB | 24 to 67 MiB | 17 to 61 MiB | 17 to 124 MiB |
| `zf-scheduler` | 512 MiB | 1 MiB | 1 MiB | 1 to 2 MiB | 2 MiB |

`zf-db` sits at its 10 GiB limit in every window because the page cache fills it; its anon RSS grew from 1.6
to 2.0 GiB as connections and the database grew. The app roles used under 10% of their limits.

Where the database CPU goes at W3 (D, cgroup cores times the share of on-CPU samples), in ms per applied item:
events 0.448, resolve 0.279, reevaluate 0.152, background 0.166, catalog 0.103, job queue 0.051, sync ledger
0.113, flows 0.067, all other paths about 0.4, total 1.78.

## 5. Ingest API latency

Server histograms, bucket-interpolated (M). Data: [latency.json](data/x5-1.0.0/latency.json).

| Window | `/api/v1/ingest/batch` p50 / p95 / p99 ms | `/hooks/{id}` p50 / p95 / p99 ms | Non-202 |
| --- | --- | --- | --- |
| W1 | 3.3 / 9.6 / 22.3 | 2.7 / 7.1 / 16.6 | 0 |
| W2 | 11.9 / 72.7 / 95.0 | 7.4 / 62.4 / 92.8 | 0 |
| W3 | 19.4 / 81.6 / 97.8 | 7.7 / 73.5 / 95.7 | 0 |
| P1 | 20.2 / 81.4 / 97.7 | 7.8 / 65.2 / 94.0 | 0 |
| P12 | 20.7 / 82.2 / 101.3 | 8.1 / 55.8 / 94.2 | 0 |

At W3 the ingest API p50, p95 and p99 were 19, 82 and 98 ms. The p99 sits next to a histogram bucket edge near
100 ms, so read it as about 100 ms. There were no non-202 responses in any window: no 429 and no 5xx. The
company-changes and forms generators fell behind (changes sent 302/s of 520/s; forms 121/s of 130/s at W3),
which the report attributes to the changes source database, not the app.

## 6. Hot statements

`pg_stat_statements` by total time at W3 (M; execution time includes lock waits). All five windows are in
[pgss-top10.json](data/x5-1.0.0/pgss-top10.json).

| # | Total s (share) | Calls | Mean ms | Statement (shortened) |
| --- | --- | --- | --- | --- |
| 1 | 1,453 (15.5%) | 87,351 | 16.6 | `WITH locked AS MATERIALIZED (SELECT ... FROM people WHERE id = ANY($2) ORDER BY id FOR UPDATE) ...` |
| 2 | 921 (9.8%) | 173,018 | 5.3 | `SELECT id, merged_into, created_at FROM people WHERE workspace_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE` |
| 3 | 772 (8.2%) | 57,540 | 13.4 | `SELECT id FROM lists WHERE workspace_id = $1` |
| 4 | 548 (5.8%) | 122,650 | 4.5 | `SELECT s.person_id, w.key, s.id, s.source, s.traits -> w.key FROM source_records s JOIN unnest(...)` |
| 5 | 304 (3.2%) | 167,679 | 1.8 | `INSERT INTO decision_records ... SELECT ... FROM unnest` |
| 6 | 288 (3.1%) | 28,553 | 10.1 | `UPDATE lists SET member_count = member_count + $3 WHERE workspace_id = $1 AND id = $2` |
| 7 | 245 (2.6%) | 25,644 | 9.5 | `SELECT id FROM people WHERE workspace_id = $1 AND id = ANY($2) ORDER BY id FOR KEY SHARE` |
| 8 | 229 (2.4%) | 48,493 | 4.7 | `SELECT id FROM lists WHERE workspace_id = $1 AND id = $2 FOR KEY SHARE` |
| 9 | 202 (2.1%) | 193,819 | 1.0 | `INSERT INTO events (...)` |
| 10 | 187 (2.0%) | 56,445 | 3.3 | `UPDATE event_dedupe d SET duplicates = d.duplicates + v.repeats FROM unnest(...)` |

The top two are the people row locks (`FOR UPDATE`) that every drain batch takes; a mean of 5 to 17 ms for a
lock-then-read is mostly waiting, not work (D). The third is a `lists` read and the sixth the `member_count`
update, both on hot list rows, at 13.4 and 10.1 ms mean. These match the Lock-wait attribution to the resolve
path (4.6 backends at W3). Total statement time was 9,393 s in a 610 s window, about 15 backends busy
including waits (M). This page reports the measurement and proposes no fix.

## 7. Growth

Method (D from M counters): `n_tup_ins` and `n_tup_del` per window from `pg_stat_user_tables`, live rows at
the start and end, and relation sizes (heap plus indexes, relations over 1 MB) at the end of every window.
"Per day at the peak rate" multiplies the W3 rate (the daily-curve peak, 6,500 events/s in total) by 86,400.
"At the daily mean" divides that by 1.862. Bytes per row is the size at the end of P12 divided by live rows
then. W3 inserts include draining the ramp backlog, so rates run a little above the offer. Data:
[growth.json](data/x5-1.0.0/growth.json),
[growth-rows-and-sizes.txt](data/x5-1.0.0/growth-rows-and-sizes.txt),
[relsizes-P12-end.tsv](data/x5-1.0.0/relsizes-P12-end.tsv).

| Table | Inserts/s at W3 | Rows/day at peak | Bytes/row | GB/day at peak | GB/day at daily mean |
| --- | --- | --- | --- | --- | --- |
| `events` | 4,600 | 397M | 645 | 256 | 138 |
| `event_dedupe` | 4,371 (deletes 0.16/s) | 378M | 297 | 112 | 60 |
| `decision_records` | 1,869 | 161M | 508 | 82 | 44 |
| `step_executions` | 132 | 11.4M | 334 | 3.8 | 2.1 |
| `segment_members` (history) | 138 | 11.9M | 371 | 4.4 | 2.4 |
| `enrollments` | 27 (83 at P1) | 2.3M (7.2M at P1) | 1,030 | 2.4 | 1.3 |
| `messages` | 63 | 5.5M | 1,688 | 9.2 | 5.0 |
| `person_changes` | 1,221 in, 947 out | not additive (the table is drained; net is a backlog artifact) | 311 | not additive | not additive |
| `sync_runs` | 0.00 | about 300 | n/a | 0 | 0 |

Other growing tables (D, same method): `source_records` 469/s (38 GB/day at peak), `people` 429/s (40 GB/day),
`identities` 660/s (18 GB/day), `delivery_attempts` 63/s (2.1 GB/day), `list_member_changes` 69/s
(1.7 GB/day). `ingest_inbox` and `jobs` churn (inserts roughly equal deletes) and show only the backlog.

- **Total database size (M).** 11.65 GB (sum of relations over 1 MB, end of W0) to 35.08 GB (end of P12);
  `pg_database_size` was 35.3 GB at the end. That is +23.4 GB in 4,334 s, **19.5 GB per hour of load** (M, D).
  At that pace it is about 467 GB a day, and about 250 GB a day at the daily mean (D).
- **Bytes per event.** `events`, `event_dedupe` and `decision_records` together come to about 1.1 KB per
  event, against the profile's older estimate of 580 bytes.
- **Retention was not reached.** Retention in the sim is 2 days (`SIM_RETENTION_DAYS=2`, passed as
  `events.retention_days`), which the 2-hour run never reached, so `event_dedupe` deletes were near zero: 0.35,
  0.34, 0.16, 0.12 and 0.13 per second against inserts of 1,363, 2,992, 4,371, 3,215 and 4,176 per second (M).
  The retained tables therefore grew by their full insert rate.
- **The stack grew during the run.** It started at 501,786 people at the ready gate and ended at 1.62M (M),
  because the generators create people (429 inserts/s at W3). Later windows therefore ran on a bigger
  database. Block reads per item rose from 1.8 at W3 to 4.1 at P1 and 3.5 at P12, and the buffer hit ratio
  fell from 0.991 to 0.984 (M).
- Sizes at the end of P12 (M): `events` 11.6 GB, `decision_records` 6.9 GB, `source_records` 5.7 GB,
  `event_dedupe` 4.8 GB, `people` 2.1 GB, `identities` 1.0 GB.

## 8. The connection settings: inconclusive

Two optional database-URL settings were tried:

- **P1** is `statement-cache-capacity=512` on every role.
- **P12** adds `options=-c plan_cache_mode=force_generic_plan` on workers only, never on `serve`.

| Run | Build and database | W3 (no setting) | P1 | P12 |
| --- | --- | --- | --- | --- |
| This run | 1.0.0, 0.5M to 1.6M people | 4,475/s at 1.78 ms/item | 3,230/s at 2.46 | 4,151/s at 1.91 |
| Gate 1 | `c628480f` (pre-release), 1.4M to 2.8M people | 3,190/s at 2.49 | 4,048/s at 1.97 | 4,750/s at 1.65 |

(applied/s at DB ms per item, M and D). The two runs disagree: gate 1 improved with each setting, this run did
not. Both ran in a fixed order, W3 then P1 then P12, on a growing database, with different starting backlogs
and different database sizes, so neither isolates the setting. Gate 1 also saw P12's event-count full refresh
take 112 s against 92 s, one sample each. This page does not recommend P1 or P12 as a default or as a measured
gain; measure them on your own workload.

This run checked the settings on the container URLs only (M): after each recreate, `ZIRV_FLOW_DATABASE__URL`
carried them as intended, with no `options` on `serve` or the scheduler, and the server accepted the option form
over `PGOPTIONS`. `plan_cache_mode` on a live worker connection and plans per call for a hot statement were not
measured.

## 9. Earlier runs

Pre-release history on the same stack, from earlier rounds. Neither is the published 1.0.0, and the people
count confounds the comparison: window 1 started from a fresh 500k people, gate 1 from a database already grown
to 1.4M to 2.8M people (M).

| Window | Build | Offered/s | Applied/s | Inbox slope /s | DB cores (throttled s/s) | DB ms/item (D) |
| --- | --- | --- | --- | --- | --- | --- |
| window 1 W1 | `d84b0a57` | 1,001 | 1,060 | 0 | 4.88 (0.30) | 4.60 |
| window 1 W2 | `d84b0a57` | 2,524 | 2,140 | +480 | 8.00 (9.75) | 3.74 |
| window 1 W3 | `d84b0a57` | 4,461 | 2,155 | +2,441 | 7.99 (9.46) | 3.71 |
| gate 1 W1 | `c628480f` | 981 | 1,053 | 0 | 5.70 (1.46) | 5.41 |
| gate 1 W2 | `c628480f` | 2,512 | 2,629 | -3 | 7.86 (5.81) | 2.99 |
| gate 1 W3 | `c628480f` | 4,434 | 3,190 | +1,434 | 7.93 (8.04) | 2.49 |
| gate 1 P1 | `c628480f` | 4,427 | 4,048 | +612 | 7.96 (6.79) | 1.97 |
| gate 1 P12 | `c628480f` | 4,434 | 4,750 | -39 | 7.83 (5.58) | 1.65 |

On 1.0.0, W3 applied 4,475/s against 3,190/s in gate 1 on the same offer, and cost less per item at W1 (3.88
against 5.41 ms) and W3 (1.78 against 2.49 ms). The code also changed between builds, and the database was
smaller in this run, so the report cannot split the gain between the two.

## 10. What this does not establish

- Reference-hardware behaviour. One development machine with CPU and memory caps ran it, and the re-run on
  reference hardware is [#606](https://github.com/Glubiz/zirv-flow/issues/606).
- A steady state at the peak. W3 ran 610 s with a standing backlog, and 2 hours is far short of a day.
- The effect of P1 or P12 (section 8), or of any database setting.
- Behaviour at the retention horizon: the 2-day retention was never reached, so retention deletes at load are
  not measured.
- Whether more database memory helps. The page cache filled the 10 GiB container and the hit ratio fell as the
  database grew, but no run varied the memory.
- Delivery to real mail providers or destinations. The flows' sends went to the sim's fixtures (about 63
  messages/s at W3, M), and this page does not report send latency.
- Any fix for the lock contention in section 6.

## 11. Data files

All in [data/x5-1.0.0/](data/x5-1.0.0/):

- [windows.csv](data/x5-1.0.0/windows.csv): one row per window, with rates, inbox, CPU and latency.
- [containers.csv](data/x5-1.0.0/containers.csv): CPU, throttling and memory per container per window.
- [growth.json](data/x5-1.0.0/growth.json): the growth table's inputs and results.
- [growth-rows-and-sizes.txt](data/x5-1.0.0/growth-rows-and-sizes.txt): inserts, deletes, live rows and sizes per table per window.
- [latency.json](data/x5-1.0.0/latency.json): ingest API and hook latency and statuses.
- [pgss-top10.json](data/x5-1.0.0/pgss-top10.json): top 10 statements by total time, per window.
- [relsizes-P12-end.tsv](data/x5-1.0.0/relsizes-P12-end.tsv): relation sizes at the end of P12.
- [stack.txt](data/x5-1.0.0/stack.txt): PostgreSQL version and settings read from the server, and the VM.
