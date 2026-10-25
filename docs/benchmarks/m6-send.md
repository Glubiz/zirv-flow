# M6 send-throughput benchmark

Issue #233, exit criterion 11 of milestone 6: architecture section 19, row "Send throughput | 300
messages/s per deployment, provider limits permitting". Measured on the published image
`ghcr.io/glubiz/zirv-flow:0.116.0` (digest
`sha256:54e150a68bc6c8e8e0cc5f93ee2f3edec96ff3c4e360290797ab7d400ef12054`, built from main `05c3626e`).
Raw data is in [`data/m6/`](data/m6/); the scripts are in `scripts/bench/m6/` and the tooling is
`sendbench` in `tools/loadgen` (runbook: `scripts/bench/README.md` section 11).

**This is a development machine, not reference hardware, and the results are provisional until the M8
hardware run.** The operator approved this heavy Docker run and lifted the CPU cap for it (no cargo job
or thread limits; the session stayed pinned to the E-cores; the sink container was given CPUs 4 to 7
and no E-core pin).

**The sink stands in for provider limits.** The SMTP destination delivers into a local sink that accepts
every message at once, so this measures the product's own send path, not what a provider would accept.

**Every result includes DKIM signing.** The SMTP destination refuses a message without a signer, so
"DKIM off" cannot be run (`DKIM=off` exits 3 in `run.sh`). The signing cost cannot be separated.

## 1. Summary

```
Target:  300 messages/s per deployment (architecture 19), the sink standing in for provider limits
Result:  184.2 messages/s over the steady window at best (variant c: 4 worker processes, block template);
         82.2 messages/s with one worker process
Verdict: missed (61% of the target at best)
```

Set 2 is the headline set: statistics refreshed (`ANALYZE`) and the bench workspace's events deleted
before every run, so the variants are comparable (section 2). Window rate is messages per second at the
sink over the steady window (the middle 80% of the unique deliveries; every window lasts 3.9 to 4.4 minutes).

| Variant | Workers | Template | Destination rate | Limits | People | Window rate | Busiest second | Verdict vs 300 |
|---|---|---|---|---|---|---|---|---|
| a | 1 | blocks | 1000/s | off | 25,000 | **82.2/s** | 100 | missed |
| b | 1 | HTML | 1000/s | off | 25,000 | **85.4/s** | 104 | missed |
| c | 4 | blocks | 1000/s | off | 60,000 | **184.2/s** | 256 | missed |
| d | 4 (best of a to c) | blocks | 300/s | off | 60,000 | **181.4/s** | 253 | missed (the bucket did not hold the rate; it never reached 300) |
| e | 1 | blocks | 1000/s | on | 25,000 | **80.4/s** | 97 | missed |
| f | 1 (repeat of a) | blocks | 1000/s | off | 25,000 | **80.0/s** | 96 | missed |

The repeat (f against a) differs by 2.7%; limits on (e) cost about 2% against a and f, inside that
noise. HTML against blocks (b against a) is about 4% faster, again close to the noise, so the render path
is not a limit (section 4).

| Variant | Delay p50 | p95 | p99 | max | `delivery.send` claim lag p50 / p95 | Queue max depth / age | Render wait p95 | Render restarts |
|---|---|---|---|---|---|---|---|---|
| a | 18.9 s | 28.0 s | 33.7 s | 37.5 s | 7.6 s / 17.3 s | 1,442 / 22 s | at most 5 ms | 0 |
| b | 17.7 s | 23.3 s | 29.7 s | 32.3 s | 6.9 s / 14.6 s | 1,672 / 19 s | at most 5 ms | 0 |
| c | 8.2 s | 10.7 s | 11.2 s | 11.7 s | 2.7 s / 4.8 s | 893 / 5 s | at most 5 ms | 0 |
| d | 8.7 s | 12.3 s | 15.1 s | 16.4 s | 3.0 s / 9.3 s | 1,297 / 5 s | at most 5 ms | 0 |
| e | 19.7 s | 31.3 s | 36.1 s | 38.8 s | 8.0 s / 21.5 s | 1,548 / 21 s | at most 5 ms | 0 |
| f | 19.5 s | 26.8 s | 32.7 s | 35.5 s | 7.7 s / 14.9 s | 1,470 / 21 s | at most 5 ms | 0 |

The delay is enrollment (`enrollments.created_at`) to the sink, all people enrolled by one manual start for
a list, so it is mostly queueing behind the other people of the run, not a latency at a steady
arrival rate. Quantiles from the exposed histogram buckets are estimates (claim lag, render wait); the
delay percentiles are exact. The render pool had 2 children in every run (`render.pool_size`).

| Variant | Database CPU mean / max | Sink CPU mean / max | Worker CPU, mean per process |
|---|---|---|---|
| a | 298% / 333% | 5% / 10% | 50% |
| b | 273% / 318% | 4% / 9% | 61% |
| c | 1,011% / 1,227% | 18% / 31% | 49% (4 processes) |
| d | 996% / 1,238% | 18% / 31% | 48% (4 processes) |
| e | 294% / 323% | 6% / 11% | 54% |
| f | 292% / 320% | 5% / 12% | 53% |

CPU is `docker stats`, percent of one core, inside the steady window (about 40 samples, a sample about
every 6 s). PostgreSQL had cpuset 8 to 31 (24 cores); the app had cpuset 0 to 3 (4 cores, shared by all
worker processes, serve and the scheduler); the sink ran on CPUs 4 to 7 and never neared one core.

## 2. Method

- **Stack.** The M4 10 M-people database (compose project `zirv-flow-m4bench10m`, volumes kept), migrated
  from 0.76.0 to 0.116.0 by the image: 17 migrations (0043 to 0059) applied in 0.23 s (about 0.26 s of
  recorded execution time, `_sqlx_migrations`). App tier M (cpuset `0-3` for serve, worker and scheduler;
  worker concurrency 4 per process; memory worker 4 GiB), PostgreSQL 16.15 on cpuset `8-31`
  (`shared_buffers=4GB`, `effective_cache_size=16GB`, `max_wal_size=8GB`, `synchronous_commit=on`, stock
  otherwise; `/dev/shm` 1 GB, the M5 value `BENCH_PG_SHM=1gb`). Host: 13th Gen Intel Core i9-13900K,
  Docker Desktop 29.2.1 (32 CPUs, 31 GiB), WSL 3.0.3.0 (`data/m6/stack.txt`). The database holds 10 M
  people, 76 M events and 30 M decision records of earlier benchmarks in other workspaces, and is 76 GB.
- **Scenario.** `setup.sh` creates through the API a workspace, a sending domain with a sender identity, an
  `smtp` credential and destination (STARTTLS, the sink's authority as `ca_certificate`), a published block
  template (heading, text, button, unsubscribe link) and a published HTML template, and an ingest key.
  `seed.sh` identifies 70,000 people with an email address through the ingest batch endpoint and subscribes
  them with the consent import: no SQL writes. `run.sh` creates a list, adds the run's people, publishes a
  manual-trigger flow (`action.send` then exit) and starts it for the list (the product path of the M5
  bench).
- **SQL-seeded.** One thing only: the sending domain's verification. The API path needs the DNS records of a
  real domain, so the records are set to `ok` (observed text equal to the expected text) and the domain to
  `verified` with the statements the verify job itself runs (`store/sending/verify.rs`); the leader's
  re-check is pushed to 720 hours. Nothing else is SQL: not the people, consent, flows or enrollments.
- **Sink.** `sendbench sink` in a container on the project's network (`zirv-m6-sink`, CPUs 4 to 7),
  STARTTLS with a generated authority, any login accepted, one JSON line per accepted message with the
  arrival time. The egress policy allows the Docker private ranges (`compose.m6.yaml`). The released
  image refuses plaintext SMTP, so the destination uses `tls: starttls` and the sink's authority.
- **Why a container, not the host.** The first sink ran on the host and was reached as
  `host.docker.internal`. Through Docker Desktop's host path new connections stalled for 10 s or more even
  at 2 parallel connections (a loop of connect, `QUIT`, close from a container: 3 stalls of 12 s per worker
  in 30 s; 8 parallel: 3 each), while the same loop against a sink container had 0 stalls and a worst
  connect of 5 ms at 4 parallel (10,300 connections per worker in 30 s). The host sink gave 33 to 108
  messages/s for the wrong reason (`retryable` outcomes, timeouts of 10 s), so its runs are discarded.
  This was a measurement defect, not a product finding.
- **Variants.** Environment switches of `run.sh`: `WORKERS`, `TEMPLATE`, `DEST_RATE`, `LIMITS` (a
  frequency cap of 1,000,000 per day and quiet hours on a weekday three days off, so the gates run and
  block nothing). A destination rate of 1000 messages/s is "above the target"; 300 is "at the target".
  Each run recreates the worker containers, so the metrics start at zero, and the workers share the app
  cpuset, so 4 processes compare process layout, not added CPU.
- **People and windows.** N is 25,000 (a, b, e, f) or 60,000 (c, d) so that every steady window lasts
  at least 3 minutes (3.9 to 4.4 minutes). The rate counts distinct recipients that appear in the run's
  `enrollments.csv`; duplicates and unmatched arrivals fail the run (there were none: 0 duplicates and 0
  unmatched in every run reported here). A run starts only when no `delivery.send` job is `ready` or
  `running`.
- **Clocks.** The database clock was 0.50 to 0.58 s ahead of the host clock (measured before and after
  each run, the tightest of 7 brackets; `docker exec` makes the bracket 1.2 to 1.3 s wide, so the delay
  has an uncertainty of about 1.2 s). The offset is applied to the enrollment instants. No delay was
  negative.
- **Statistics and events (set 2).** The database had no planner statistics after its earlier crash
  (`last_analyze` empty for every table), which made the people lock query 500 times slower (section 5, miss
  2). Before set 2 the whole database was analysed (`ANALYZE`, 12 s). The bench workspace's events were also
  deleted before each run (`RESET_EVENTS=1`, then `VACUUM` of the current partition), because a lookup in
  the send path costs time proportional to the events already in the workspace (miss 1), which otherwise
  makes later runs slower than earlier ones. Set 1 (section 3) was run without either, and shows the drift.
- **Metrics.** Each worker's `/metrics` was scraped at the end of the run; the destination queue gauges
  (`zirv_flow_destination_queue_depth` and `_oldest_seconds`) are the scheduler leader's, so they were
  sampled from the scheduler every second for this run's destination only (0 missed scrapes in every
  run). Database, worker and sink CPU come from `docker stats`, cut to the steady window. No `cargo.exe`
  ran inside any window (`cargo_samples_in_window=0` in each `variant.txt`).
- **The missing rate-bucket wait metric.** The product has no rate-bucket wait metric (#215 added the
  bucket, not a histogram of its waits). The proxies used: the destination queue depth and oldest age,
  the `delivery.send` claim lag and duration, and the count of deferrals in `messages.txt`. In variant d
  the bucket at 300/s was never reached (the window rate was 181/s), so the bucket did not wait.

## 3. Set 1: the first full set (no statistics, events accumulating)

Run first, on the database as it was after the crash and without the event reset. Kept because it shows
the drift. Same variants and sizes.

| Run | Window rate | Delay p99 | Claim lag p50 | Database CPU mean |
|---|---|---|---|---|
| a (1 worker, blocks) | 54.6/s | 27.1 s | 10.0 s | 324% |
| b (1 worker, HTML) | 51.9/s | 29.2 s | 10.0 s | 332% |
| c (4 workers, blocks) | 138.0/s | 19.0 s | 3.8 s | 1,224% |
| d (4 workers, bucket 300) | 123.8/s | 22.8 s | 4.1 s | 1,270% |
| e (1 worker, limits on) | 37.6/s | 51.8 s | 11.8 s | 347% |
| f (1 worker, repeat of a) | 36.1/s | 66.1 s | 14.4 s | 349% |
| g (f after `VACUUM (ANALYZE)` of the send tables) | 38.2/s | 53.2 s | 11.2 s | 347% |

The repeat f is 34% below a: the set is not stable, because the environment changed from run to run
(stale statistics, events accumulating in the workspace). `VACUUM` of the send tables (g) did not help.
The cause was found with the active-query samples of section 5. Set 1 is not used for the verdict.

## 4. The limiting resource

The database, through the cost of each message in it. Established:

- With 1 worker process the worker itself is at about 50% of a core, the sink at 5% and the database at 3
  cores: not CPU-saturated anywhere, but the 4 job slots of one process cannot go faster than the database
  answers them (the claim lag of 7 s and a queue of 1,400 show the slots are the throttle).
- With 4 processes (16 slots) the rate rises 2.24 times (82.2 to 184.2/s) and the database goes from 3 to
  10 cores of 24, still not saturated, while each worker stays at 49% and the app cpuset is not full.
  The database spends about 36 ms of CPU per message with 1 process and 55 ms with 4.
- Sampling `pg_stat_activity` during a 4-process run (`data/m6/diag-2-active-queries.txt`, 1,010 active
  samples) puts **68% of all active database time in one statement**, the event lookup by id of the
  flow path (`SELECT id, trace_id FROM events WHERE workspace_id = $1 AND id = ANY($2)`, miss 1), and 11%
  in commits waiting for an object lock (`Lock/object` on `COMMIT`, miss 3). The rest is the usual WAL
  write and sync waits of inserts and updates.
- Not the render pool (wait at most 5 ms at p95, the first histogram bucket, 0 restarts, and the HTML
  template is not slower than the block one), not the destination bucket (the rate never reached
  300/s), not the sink (at most 31% of a core), not the quiet hours and cap gates (about 2%, in the
  noise), not the network (a container sink; section 2).

## 5. Misses and causes

The target is missed by 39% at best. The causes, each with its own issue (the numbers are measured here):

1. **Event lookup by id costs time proportional to the events of the workspace: #296.**
   `SELECT id, trace_id FROM events WHERE workspace_id = $1 AND id = ANY($2)` has no index it can use: the
   partition primary key is `(occurred_at, id)`, so the plan walks the workspace's whole
   `(workspace_id, occurred_at)` index range and filters on `id`. With 40,000 events in the workspace one
   lookup of 1 id reads 42,260 buffers and takes 34.7 ms (`data/m6/explain-events-by-id-40000-events.txt`);
   with 274,599 events removed by the filter it read 276,701 buffers and took 45.6 ms. It is 68% of the
   active database time at 4 processes. Set 1 against set 2 shows the effect on throughput (a 54.6 against
   82.2/s on the same variant, and f 36.1/s after the events piled up). A lookup by id and time (the v7 id
   carries the time) or an index on `(workspace_id, id)` would make it constant.
2. **The people lock query without planner statistics: #297.**
   `SELECT id, merged_into FROM people WHERE workspace_id = $1 AND id = ANY($2) ORDER BY id FOR UPDATE` chose
   `people_workspace_idx` and filtered 69,999 rows to find 1 (76,476 buffers, 41.5 ms); after `ANALYZE people`
   it uses `people_pkey` (0.075 ms). The bench database had lost its statistics (a crash; autovacuum's
   analyze threshold on a 10 M-row table is 1 M changed rows, so it never refreshed them). This is partly
   an operations matter (run `ANALYZE` after a restore), but a send path whose cost goes from 0.075 ms to
   41 ms per message on a statistics gap deserves a plan that does not depend on it.
3. **Commits waiting for an object lock under parallel workers: #298.** 108 of 1,010 active
   samples (11%) at 4 processes were `COMMIT` in `Lock/object`, which a single process does not show. The
   likely candidate is the database-wide lock PostgreSQL takes at commit for a transaction that issued a
   `NOTIFY`, which every job enqueue does (`pg_notify` in the job store); the sampling does not name the
   lock, so the cause is not established. It is why 4 processes give 2.24 times, not 4 times, the rate
   of 1.

With the first miss removed the others become the limit; the rate reachable at 300/s is not extrapolated
here.

## 6. Caveats

- One host, one database, no other load; Docker Desktop on a workstation. The database carries 76 GB of
  earlier benchmark data in other workspaces, which makes statistics and cache behaviour unlike a fresh
  deployment.
- Everything is one `Start` of a list: the people are enrolled in a burst, so the delay is queueing
  delay. A paced arrival would give different delays at the same throughput.
- The workers share the 4-core app cpuset, so variant c is the process layout, not more CPU.
- DKIM signing is in every result and cannot be taken out (the destination refuses an unsigned message).
- The destination's token bucket was above the achieved rate in every run, so it never limited
  anything; variant d therefore does not show a bucket "holding" 300/s.
- Set 2 deleted the bench workspace's events between runs and refreshed statistics; a production
  deployment would have neither the deleted events nor the missing statistics, but would have the events
  growing (miss 1).
- Clock offset between the database and the host is 0.5 s with an uncertainty of about 1.2 s; the delay
  percentiles are good to about that.
- Quantiles of the claim lag and render wait are estimated from histogram buckets.

## 7. Script defects found and fixed while proving the scripts

- The people-list request body went through the command line (`jq --argjson`) and exceeded the Windows
  argument limit; it is now a file.
- The fixture domains `bench-send.example` and `bench.example` are refused as special-use; they are now
  `bench-send.example.com` and `bench.example.com` (nothing is ever sent there).
- The public URL must be `https`: the renderer fails every block email with `render_error` when the
  tracking base is plain `http` (all 2,000 messages of the first light run); the default is now
  `https://flow.bench.example.com`.
- `extra_hosts: host-gateway` made `host.docker.internal` resolve to an IPv6 unique-local address that the
  egress policy refuses; removed, and then the host sink was replaced by the container sink (section 2).
- Worker containers are found with `compose ps` and `docker port`, not by replica index (indexes are not
  1..N after a scale down).
- `VACUUM` and `COUNT` of the events used parallel workers and exhausted the 1 GB `/dev/shm`; they now run
  without parallel workers.
- The product has no workspace delete, and a cascading `DELETE FROM workspaces` of the 70,000-person
  workspace ran for 21 minutes (a foreign key check per deleted row) before it was cancelled. `clean.sh`
  now deletes every row of that one workspace table by table with the foreign key triggers off for the
  session, then the workspace (20 s). The 10 M people of the other workspace, the 11 enrollments and the
  3 jobs of the M5 workspace are untouched (checked by count).
- The report generator was extended while proving: per-container CPU over the steady window, the
  scheduler queue samples with a count of missed scrapes, and the clock bracket; a run that matches no
  enrollment is reported invalid.

## 8. Reproduce

```sh
docker pull ghcr.io/glubiz/zirv-flow:0.116.0
cargo build --release -p zirv-flow-loadgen
export BENCH_IMAGE_TAG=0.116.0
scripts/bench/m6/stack.sh up
scripts/bench/m6/sink.sh build && scripts/bench/m6/sink.sh start
scripts/bench/m6/setup.sh
SEED_PEOPLE=70000 scripts/bench/m6/seed.sh
# database statistics: ANALYZE once; then per run:
RESET_EVENTS=1 PEOPLE=25000 WORKERS=1 TEMPLATE=blocks scripts/bench/m6/run.sh a-1w-blocks
RESET_EVENTS=1 PEOPLE=60000 WORKERS=4 scripts/bench/m6/run.sh c-4w-blocks
M6_CONFIRM_CLEAN=1 scripts/bench/m6/clean.sh && scripts/bench/m6/stack.sh stop
```

`shellcheck` was not installed; `bash -n` passes for every script.
