# Benchmark runbook

How to run the ingestion benchmarks of issue #47 (architecture section 19 targets and OQ-1). The
tools here only measure; the results, the environment and the pass or miss calls go in
`docs/benchmarks/m2-ingestion.md`, and every raw report JSON in `docs/benchmarks/data/`.

| Tool | What it is |
|------|------------|
| `tools/loadgen` | `loadgen`, a paced, seeded load generator that writes a JSON report; `sendbench`, the SMTP sink, people seeder and send report |
| `scripts/bench/applied-latency.sql` | inbox-to-applied p50, p95 and p99 for track and identify, from the database |
| `scripts/bench/compose.bench.yaml` | compose override: separate serve, worker and scheduler, PostgreSQL published on 55432 |
| `bench-no-decisions` | cargo feature of `zirv-flow-store`: decision appends become no-ops (benchmark builds only) |
| `scripts/bench/oq1/` | the OQ-1 trait storage benchmark (SQL and `run.sh`) |
| `sendbench` (in `tools/loadgen`), `scripts/bench/m6/` | the send-throughput benchmark (section 11) |
| `tools/renderbench`, `scripts/bench/m7/render.sh` | the render benchmark through the real render pool, no database (section 12) |

Not reference hardware: record the CPU, RAM, disk, OS, Docker resources and `SHOW ALL` of the
PostgreSQL that served the run with every result, and say the targets are re-checked on reference
hardware before 1.0. A missed target is a GitHub issue, not a tuned target.

## 1. Build and start

```sh
cargo build --release -p zirv-flow-loadgen          # target/release/loadgen
docker compose -f docker-compose.yml -f scripts/bench/compose.bench.yaml up -d --build --wait
curl -fsS localhost:8080/readyz
```

`compose.bench.yaml` replaces the quickstart's single `all` process with `serve`, `worker` and
`scheduler`, and publishes PostgreSQL 16 on `127.0.0.1:55432`. The worker's metrics are on
`localhost:9100/metrics`, the scheduler's (including `zirv_flow_ingest_inbox_age_seconds`) on
`localhost:9101/metrics`, the API's on `localhost:8080/metrics`. The default per-key limit is
10,000 items/s with a burst of 20,000; for run 4 start the stack with
`BENCH_RATE_PER_KEY=20000 BENCH_BURST_PER_KEY=40000` and say so in the report. Between runs reset the
database with `docker compose -f docker-compose.yml -f scripts/bench/compose.bench.yaml down -v`
and start again, so a run never sees an earlier run's data.
`compose.bench.yaml` sets the Compose project name `zirv-flow-bench`, so `down -v` removes only this
stack's volumes and never the quickstart's `postgres-data` and `zirv-flow-secrets`, whatever the
checkout directory is called. Always pass both `-f` files (the name lives in the second); the
containers are `zirv-flow-bench-<service>-1`.

## 2. Create the `ingest` API key

There is no CLI for API keys; they are created over the API by a signed-in workspace admin (docs/people.md,
"Ingestion"). The CLI creates the owner (the password is one line of standard input), then:

```sh
c="docker compose -f docker-compose.yml -f scripts/bench/compose.bench.yaml"
$c exec -T zirv-flow zirv-flow admin create-owner --email bench@example.com   # type the password

base=http://localhost:8080/api/v1
headers=$(mktemp)
csrf=$(jq -n --arg e bench@example.com --arg p "$BENCH_PASSWORD" '{email: $e, password: $p}' \
  | curl -fsS -D "$headers" -X POST "$base/session" -H 'content-type: application/json' --data-binary @- \
  | jq -r .csrf_token)
session=$(tr -d '\r' < "$headers" | sed -nE 's/^[Ss]et-[Cc]ookie: (zf_session=[^;]+).*/\1/p' | head -n1)

ws=$(curl -fsS -X POST "$base/workspaces" -H "cookie: $session" -H "x-csrf-token: $csrf" \
  -H 'content-type: application/json' -d '{"name": "bench"}' | jq -r .id)
curl -fsS -X POST "$base/workspaces/$ws/api-keys" -H "cookie: $session" -H "x-csrf-token: $csrf" \
  -H 'content-type: application/json' -d '{"name": "loadgen", "scopes": ["ingest"]}' \
  | jq -r .key > bench.key        # shown once; do not commit bench.key
```

The sign-in, workspace and key calls are the ones the CI container smoke test makes (and they were
exercised against a real server for this runbook). Ingestion needs no CSRF token, only
`Authorization: Bearer <key>`, which `loadgen` sends.

## 3. loadgen

```
loadgen --url http://localhost:8080 --key-file bench.key --mode identify|track|batch
        --rate <items/s> --duration <s> [--burst-rate <items/s> --burst-duration <s>]
        [--people <n>] [--batch-size <n>] [--max-in-flight <n>] [--seed <n>] [--run-id <s>]
        [--out <report.json>]
```

- The base phase (`--rate`, `--duration`) runs first, then the burst phase. Either may be omitted.
- Pacing is open loop at `--rate` items per second: each request is sent at its scheduled time,
  never held back waiting for earlier responses. `--max-in-flight` (default 10,000, alias
  `--concurrency`) is only a safety cap; when it is reached the request goes out as soon as a slot
  frees and counts in `late_dispatch`.
- **Which latency is which.** `latency_from_schedule` runs from each request's scheduled due time
  to its final response, so dispatch lag and `Retry-After` waits are included and a stalled server
  cannot under-report. The API target (p99 under 100 ms at the sustained rate) is judged on this
  figure. `service_latency` is the send-to-response time of the last attempt only, and `dispatch_lag`
  is the final send minus the scheduled time, so schedule is about dispatch lag plus service time; report
  them beside it, never instead of it. Each holds one sample per request at its final outcome of any
  status (202, 4xx, 5xx, timeout, 429 retries exhausted), so overload moves the percentiles.
- A 429 sets one shared pause (the largest `Retry-After` seen, 1 to 60 s). The refused requests go
  back to the dispatcher, which resends them after the pause, ahead of new ones and paced at the
  target rate (up to 8 sends each); the pause is not made up afterwards, so there is no stampede.
- Payloads depend on `--seed` and the item number alone. Every `messageId` is `lg-<run-id>-<n>`;
  `--run-id` defaults to the start time, so a rerun does not dedupe against an earlier run (pass
  the same one to repeat a run exactly). Items carry no `timestamp`: the server stamps arrival,
  which `applied-latency.sql` relies on for identify.
- The report has `total` and one entry per phase with `achieved_rate`, `status`
  (`accepted_202`, `rate_limited_429`, `server_5xx`, `other`, `transport_errors`), `retries_after_429`,
  `late_dispatch`, `gave_up_after_retries` (still 429 after the last send; these have latency samples),
  `abandoned_requests` (queued retries dropped at the wall-time limit; no sample) and the three latency blocks above (`p50_ms`, `p95_ms`, `p99_ms`, `max_ms`). Only plain `http://` is supported.

### Debugger latency (M8, issue #394)

Start the watcher before ingestion with a workspace API key carrying `read` scope (the ingest
key above has only `ingest`). Use `--clients 1` or `20`; the stream filters to `source=api`,
reconnects with `Last-Event-ID`, and honours a 429's `Retry-After`.

```sh
loadgen debugger --url http://localhost:8080 --key-file read.key --workspace "$ws" \
  --clients 20 --duration 600 --out receive.jsonl
psql -X "$DATABASE_URL" -v receive_file="$PWD/receive.jsonl" \
  -f scripts/bench/m8/debugger-latency.sql
```

The JSONL holds one `item` per receipt (`client`, nullable `event_id`, `received_at` in UTC Unix
microseconds), `started`/`finished` clock boundaries and per-client gap/skipped/status summaries.
Run SQL after the inbox drains, on the watcher's host with `python3` available and stable clocks;
it fails unless that host's short name matches the `host` in the `started` record, then samples that host around database `now()` and fails if skew plus uncertainty, or the run's wall
versus monotonic clock step, exceeds 50 ms. Saved boundaries alone cannot establish clock skew.
SQL reports first-receipt nearest-rank p50/p95/p99/max and delivered share per client and overall:
distinct client/event pairs divided by clients times stored API events arriving during the run.
Leave a drain window before watchers stop; non-stored outcomes are excluded from that denominator.
The target is p95 < 2 s; the M8 driver below runs the 2,000 events/s scenario.
The driver scrapes `zirv_flow_debugger_clients` and `zirv_flow_debugger_delivery_seconds`.

## 4. Applied latency and backlog drain

After a run, once the inbox is empty:

```sh
$c exec -T postgres psql -U zirv -d zirv_flow -f - < scripts/bench/applied-latency.sql
# only items that arrived after a moment (UTC), when the database holds earlier runs:
$c exec -T -e PGOPTIONS="-c bench.since=2026-10-11T18:00:00Z" postgres \
  psql -U zirv -d zirv_flow -f - < scripts/bench/applied-latency.sql
```

It prints one row per kind with `items`, `p50_ms`, `p95_ms`, `p99_ms` and `max_ms`. Caveats, stated
in the script: the decision timestamp is the drain transaction's start, so the figure slightly
under-estimates; and identify is measured per source record (its latest change), so use `--people`
at least as large as the items sent, or the count is the number of distinct people.

Backlog drain time (run 4): note the time `loadgen` stops, then poll until the inbox is empty; the
drain time is the difference.

```sh
until [ "$($c exec -T postgres psql -U zirv -d zirv_flow -Atc 'SELECT count(*) FROM ingest_inbox')" = 0 ]; do sleep 5; done; date
```

## 5. The runs (spec D5)

Each uses a fresh stack (section 1), a fresh key (section 2) and `--out docs/benchmarks/data/<name>.json`.
Run the applied-latency script after each and save its output beside the report.

| # | Run | Command |
|---|-----|---------|
| 1 | identify, 2,000/s for 10 min | `loadgen --url http://localhost:8080 --key-file bench.key --mode identify --rate 2000 --duration 600 --people 2000000 --out docs/benchmarks/data/identify-2000.json` |
| 2 | track, 2,000/s for 10 min | `loadgen ... --mode track --rate 2000 --duration 600 --people 100000 --out docs/benchmarks/data/track-2000.json` |
| 3 | batch of 500 at 2,000/s | `loadgen ... --mode batch --batch-size 500 --rate 2000 --duration 600 --people 1000000 --out docs/benchmarks/data/batch-2000.json` |
| 4 | burst 10,000/s for 10 min, drain time | `loadgen ... --mode batch --batch-size 100 --burst-rate 10000 --burst-duration 600 --people 1000000 --out docs/benchmarks/data/burst-10000.json`, with the raised key limit; then time the drain (section 4) |
| 5 | decision overhead A/B | section 6 |
| 6 | OQ-1 | section 7 |

## 6. Decision-log overhead (run 5)

Targets: under 5 ms added transaction time at p95 and under 10% throughput loss against the same
workload with decision writes disabled.

The feature is compile-time only, and the store refuses to compile it unless `ZIRV_FLOW_BENCH_BUILD`
is set (a guard against `--all-features` and aliases). Cargo tracks that variable, so changing it
rebuilds. Build two binaries and run them natively against the compose
PostgreSQL (published on 55432), not through the image, which never carries the feature:

```sh
cargo build --release -p zirv-flow && cp target/release/zirv-flow ./zirv-flow-on
ZIRV_FLOW_BENCH_BUILD=1 cargo build --release -p zirv-flow \
  --features zirv-flow-store/bench-no-decisions \
  && cp target/release/zirv-flow ./zirv-flow-off
./zirv-flow-on admin generate-master-key --out bench-master.key
export ZIRV_FLOW_DATABASE__URL=postgres://zirv:zirv-quickstart@localhost:55432/zirv_flow
export ZIRV_FLOW_SECRETS__MASTER_KEY_FILE=bench-master.key ZIRV_FLOW_DATABASE__MIGRATE_ON_START=true
export ZIRV_FLOW_SERVER__COOKIE_SECURE=false ZIRV_FLOW_LOG__FILTER=warn
```

For each binary (`on`, then `off`), starting from `docker compose ... down -v` and
`up -d --wait postgres` only:

1. `./zirv-flow-X serve &` (migrates), create the owner, workspace and key (section 2; the owner with
   `./zirv-flow-X admin create-owner`, the API at `localhost:8080` unless `ZIRV_FLOW_SERVER__BIND` is set).
2. With no worker running, fill the inbox: `loadgen --mode track --rate 2000 --duration 300 --people 100000`.
3. `ZIRV_FLOW_OPS__BIND=127.0.0.1:9100 ./zirv-flow-X worker &` and time until the inbox is empty
   (section 4). Sustained drain throughput is items sent divided by that time.
4. Scrape `localhost:9100/metrics` before stopping the worker.

Throughput loss is `(on_time - off_time) / on_time`, with the drain times of the two builds. There is **no decision-append duration histogram** in the code today (the only related
metrics are `zirv_flow_job_duration_seconds{kind="ingest.drain"}` and `zirv_flow_job_claim_lag_seconds`).
The nearest measure of added transaction time is the p95 of the drain job's duration with and
without the feature, divided by `ingest.drain_batch_size` (default 500) rows per transaction; say
that it is a proxy. Do not read the "on" build's tests as passing under `off`: with the feature
enabled, tests that read decisions fail by design.

## 7. OQ-1 (run 6)

Needs a PostgreSQL 16 scratch database and `psql`, nothing from zirv-flow. The runner creates and
drops only the schemas `oq1_bench_a` and `oq1_bench_b`:

```sh
DATABASE_URL=postgres://zirv:zirv-quickstart@localhost:55432/zirv_flow \
  OQ1_PLAN_DIR=docs/benchmarks/data/oq1-plans \
  bash scripts/bench/oq1/run.sh > docs/benchmarks/data/oq1.json
```

Defaults: 1,000,000 people, 5 runs, 10,000 upserts. `OQ1_PEOPLE=10000` is a smoke test. With no
`psql` on the host, run it through a container: `PSQL="docker exec -i <container> psql"` and a
`DATABASE_URL` valid inside the container. Do not point it at a database that holds real data. Medians
of `exec_ms` (the plan's execution time), `wall_ms` (psql's client-side time), the planning time
and the top node's buffers are in the JSON; the last run's full plans are in `OQ1_PLAN_DIR`. The
table and index sizes of each variant are included.

Variant A needs a wrapper function (`oq1_ts`) around the timestamp cast to index it, because
`text::timestamptz` is only STABLE; it is part of A's cost and belongs in the OQ-1 record.

## 8. Capacity tiers (S, M, L) and the decision A/B

`scripts/bench/tier.sh <S|M|L> <test>` runs the capacity benchmarks at doubling sizes of the
zirv-flow stack: S is 2 vCPU / 4 GB, M 4 vCPU / 8 GB, L 8 vCPU / 16 GB. The tier budget covers
serve, worker and scheduler only (one shared cpuset `0-1`, `0-3` or `0-7`; memory worker 50%, serve
40%, scheduler 10%; worker concurrency 4 in every tier). PostgreSQL 16 is outside the budget: its
own cpuset `8-31`, no memory limit and one fixed configuration in every tier (`shared_buffers=4GB`,
`effective_cache_size=16GB`, `maintenance_work_mem=1GB`, `max_wal_size=8GB`, `checkpoint_timeout=15min`),
all in `compose.tiers.yaml`. The generator runs on the host. Build `loadgen` and the image first
(sections 1 and 3), then, per tier:

```sh
export BENCH_LOADGEN=target/release/loadgen BENCH_SCRATCH=/tmp/zirv-bench
for t in track1 identify1 track4 batch1 sustained burst; do scripts/bench/tier.sh M "$t"; done
scripts/bench/tier.sh M batch; scripts/bench/tier.sh M oq1       # tier M only
cargo test --release -p zirv-flow-store --no-run                  # once, before decappend
scripts/bench/tier.sh M decappend                                 # tier M only
```

- `track1`, `identify1` and `track4` (four workspaces, four generators at rate/4) ramp from 250 items/s
  in steps of 250 for 90 s, up to 8,000 (`BENCH_RAMP_MIN`, `BENCH_RAMP_STEP`, `BENCH_RAMP_MAX`), and stop at the first step that fails: schedule-based API p99
  under 100 ms, no 5xx or 429, inbox oldest-row age under 5 s and not growing, the generator on
  schedule (`late_dispatch` 0, achieved at least 98% of the target). A step with `late_dispatch` above 0
  is generator-bound and invalid. `BENCH_STEP_SECONDS` and `BENCH_RAMP_MAX` shorten a smoke run.
- `batch1` is a ramp in batches of 100 (identify and track mixed) from 2,000 items/s in steps of 2,000 with the
  per-key limit raised, to measure limits the single-item host path hides.
- `sustained` is track at 2,000/s for 10 minutes; `burst` is 10,000/s for 2 minutes (`BENCH_BURST_SECONDS`, default 120; 600 is the section 19 case, written as `burst600`) (batch mode, key
  limit raised to 20,000/40,000) then the drain to empty, capped at 30 minutes, which also covers a 10-minute burst (15-minute target); `batch` is 500 per
  request at 2,000 items/s for 10 minutes.
- Every test recreates the stack on a fresh volume, samples `docker stats` and the inbox, and writes
  to `docs/benchmarks/data/<tier>/`. Peak CPU is in percent of one core.

Clients on the host aim at `127.0.0.1` (`BENCH_HOST`): through the name `localhost`, Docker Desktop on Windows
makes new connections about 140 ms slower at p99 and each PostgreSQL round trip about 4.4 ms. The decision
append test (`decappend`) prints progress and stops a shape at `ZIRV_FLOW_BENCH_DECISIONS_BUDGET_S` (default 600);
the deployment-shaped figure is the same 500-insert transaction timed with `pgbench` inside the container network.

`scripts/bench/ab.sh <on|off> <n> <zirv-flow-binary> <loadgen-binary>` is the decision-log A/B:
native release builds (the `off` one built as in section 6), unconstrained, against the tier PostgreSQL;
it preloads a backlog with no worker, then times the drain to empty. Run each side at least twice.

## 9. Segment benchmarks (M4, issue #127)

`scripts/bench/m4/` measures segment preview, the trait slots, full refresh and the incremental
event-to-entry latency (architecture 8.2, 19 and 21). It uses the published image, never a local
build, in its own compose project `zirv-flow-m4bench` (so `zirv-flow-bench` and its volumes are
never touched), PostgreSQL on the tiers default cpuset and the app at tier M. Each script has a
header with its purpose, environment and usage.

```sh
docker pull ghcr.io/glubiz/zirv-flow:0.55.0                 # BENCH_IMAGE_TAG; pull_policy is never
cargo build --release -p zirv-flow-loadgen                  # for the incremental phase
scripts/bench/m4/stack.sh up                                # stack, owner, stack facts
scripts/bench/m4/setup.sh                                   # workspace, 20 traits, 5 event schemas, ingest key (API)
SEED_PEOPLE=10000000 SEED_EVENTS=50000000 scripts/bench/m4/seed.sh   # defaults 100000 / 500000
scripts/bench/m4/measure.sh all                             # or one phase: verify, preview-jsonb, index, ...
scripts/bench/m4/stack.sh stop                              # keep the volumes
```

Credentials and state (the generated owner password, the session headers, the loadgen key) live
in an untracked directory (`M4_STATE`) and are never printed. Results go to `M4_OUT` as raw JSON
and EXPLAIN plans. `measure.sh verify` checks each preview count against a hand-written SQL count
over the seeded rows. The seed writes the rows the app writes (checked against an identify and a
track sent through the API); it does not write the source-record ledger, decisions, event
deduplication rows or event message ids, which no segment query reads.

## 10. Timer benchmark (M5, issue #169)

`scripts/bench/m5/` measures timer lateness at 1,000,000 waiting enrollments (architecture 9.3 and
19) on the published image, in the compose project of the M4 10 M-people database
(`zirv-flow-m4bench10m`). It sources `m4/common.sh` and reuses `m4/stack.sh`. The waiting
enrollments are **seeded by SQL** (ruling 46fb269a) with rows copied from a sample enrolled through
the product path, so the benchmark is evidence about timers only, not about enrollment throughput.

```sh
docker pull ghcr.io/glubiz/zirv-flow:0.76.0                 # BENCH_IMAGE_TAG; pull_policy is never
cargo build --release -p zirv-flow-loadgen                  # for the loaded run
scripts/bench/m5/stack.sh up                                # the M4 stack on the M5 defaults
scripts/bench/m5/setup.sh                                   # three manual-trigger flows and sample lists (API)
scripts/bench/m5/sample.sh                                  # enroll the sample, capture its rows
RUN=1 scripts/bench/m5/seed.sh stage load vacuum verify     # 900,000 spread + 100,000 burst, then the checks
scripts/bench/m5/measure.sh start 1 noload                  # waits for the window, samples; detached
scripts/bench/m5/measure.sh wait 1 noload && scripts/bench/m5/measure.sh collect 1 noload
RUN=1 scripts/bench/m5/seed.sh clean                        # delete the run's seeded rows
scripts/bench/m5/stack.sh stop                              # keep the volumes
```

`measure.sh start RUN LABEL load` adds `tools/loadgen` at 2,000 track events/s for the window.
`seed.sh` fixes the window start (`M5_T0_LEAD_MIN` minutes ahead) when it stages; the load and the
checks must finish before it. Results go to `M4_OUT/run<RUN>-<LABEL>/`.

## 11. Send throughput (M6, issue #233)

`scripts/bench/m6/` measures `action.send` through the SMTP destination against the architecture 19
target (300 messages/s per deployment, provider limits permitting), on the published image, in the
compose project `zirv-flow-m4bench10m` with the M6 environment layer `m6/compose.m6.yaml`. The
tooling is `sendbench` in `tools/loadgen` (a sink, a people seeder and the report generator) and the
scripts; nothing under `crates/` is involved.

**Scenario.** `setup.sh` creates, through the API, a workspace, a sending domain and sender
identity, an `smtp` credential and destination on the sink (STARTTLS), a published block template
and a published HTML template, and an ingest key. **One step is SQL:** the domain verification,
because the API path needs the DNS records of a real domain; the statements are the ones the verify
job runs (`store/sending/verify.rs`), so the rows have the product's shape (records `ok` with the
expected text observed, domain `verified` with `verified_at`). `seed.sh` identifies the people with
an email address through the ingest batch endpoint (`sendbench people`) and subscribes them with
the consent import: no SQL writes. `run.sh` then creates a list and a manual-trigger flow
(`action.send` then exit) for the chosen template, adds the people to the list, starts the flow for
the list (the product path of the M5 bench) and waits for the sink.

**The sink stands in for provider limits.** It runs as a container (`zirv-m6-sink`, CPUs 4 to 7) on the
project's network, so the workers reach it as `zirv-m6-sink:2525` inside the egress allowlist of
`compose.m6.yaml` (the Docker private ranges; override with `BENCH_EGRESS_ALLOW`). A sink on the host reached
through `host.docker.internal` was tried and rejected: Docker Desktop's host path stalled new connections
for 10 s or more at 2 parallel connections. The container runs a Linux build of `sendbench` (`sink.sh
build` compiles it in the `rust:1-bookworm` image, which docker pulls if it is not local, `cargo build
--locked` downloads the crates, and the build cache is the docker volume `m6-sendbench-target`; remove it
afterwards with `docker volume rm m6-sendbench-target`). The sink container carries the label
`zirv.bench=m6-sink` and `sink.sh` removes only a container with it. The released image refuses
plaintext SMTP, so the sink offers STARTTLS with a generated authority; `sink.sh` writes its PEM to
`$M6_OUT/sink/ca.pem` and `setup.sh`/`run.sh` put it in the destination's `ca_certificate` (restarting the
sink makes a new authority: run `setup.sh` again or let `run.sh` patch it). The sink accepts any login and
logs `{"t_us", "rcpt"}` per accepted message inside the container; `M6_SINK_DELAY_MS` adds a reply delay.
The public URL of the stack must be `https` (`BENCH_PUBLIC_URL`, default `https://flow.bench.example.com`):
the renderer refuses click tracking on a plain `http` base and every block email fails as `render_error`.
Run `ANALYZE` on a restored database first, and set `RESET_EVENTS=1` on `run.sh` to delete the bench
workspace's events before a run (a lookup in the send path costs time proportional to them; see
`docs/benchmarks/m6-send.md`).

```sh
docker pull ghcr.io/glubiz/zirv-flow:<tag>                  # BENCH_IMAGE_TAG; pull_policy is never
cargo build --release -p zirv-flow-loadgen                  # loadgen and sendbench
export BENCH_IMAGE_TAG=<tag>
scripts/bench/m6/stack.sh up                                # the stack, one worker
scripts/bench/m6/sink.sh build && scripts/bench/m6/sink.sh start
scripts/bench/m6/setup.sh
SEED_PEOPLE=20000 scripts/bench/m6/seed.sh
PEOPLE=5000 scripts/bench/m6/run.sh smoke-1w                # one light run first
WORKERS=3 TEMPLATE=html DEST_RATE=300 LIMITS=on PEOPLE=20000 scripts/bench/m6/run.sh html-3w-300-limits
M6_CONFIRM_CLEAN=1 scripts/bench/m6/clean.sh                # delete the bench workspace, stop the sink
scripts/bench/m6/stack.sh stop                              # keep the volumes
```

**Variants** are environment switches of `run.sh`; each run recreates the workers so their metrics
cover that run only:

| Variant | Switch | Notes |
|---------|--------|-------|
| one worker process or several | `WORKERS=1..10`, `WORKER_CONCURRENCY` (default 4) | all processes share the tier's cpuset, so this compares process layout, not added CPU |
| DKIM | `DKIM=on` only | the SMTP destination refuses a message with no signer, so DKIM cannot be switched off; `DKIM=off` exits 3. The signing cost is in every result |
| cap and quiet hours | `LIMITS=on` | a cap of 1,000,000 per day and quiet hours on a weekday three days off: the gates run, no message is blocked |
| destination rate | `DEST_RATE=1000` (above the target) or `DEST_RATE=300` | at 300 the report allows 3% under the target, because the bucket is the limit |
| block or HTML template | `TEMPLATE=blocks` or `html` | `BENCH_RENDER_POOL` sets `render.pool_size` (default 2) in the workers |

**Measures.** `sendbench report` joins the sink log with the enrollment times (`enrollments.created_at`
by address) and prints, one line each, `Target`, `Result` and `Verdict`, then detail lines.
- Only real, unique deliveries count: the rate and "complete" use distinct addresses that appear in
  this run's `enrollments.csv`. Duplicates and arrivals with no enrollment (a previous run's
  stragglers) are counted apart and make `met` and "complete" false. `run.sh` refuses to start while
  a `delivery.send` job is `ready` or `running`, so a run never drains the previous one.
- Rate: messages per second at the sink over the steady window, from the arrival at rank 10% to the
  arrival at rank 90% (`--trim`), plus the overall rate and the busiest second.
- Delay: enrollment to the sink, nearest-rank p50, p95, p99 and max, printed `n/a` when there is
  nothing to measure. The database and host clocks are compared right before and right after the
  run (host time before, `clock_timestamp()`, host time after; the midpoint). The offset moves the
  enrollment instants onto the host clock, both offsets and the bracket width (the delay's
  uncertainty) are printed, negative delays are counted, and a delay below minus the uncertainty
  makes the run invalid. A run with 0 matched deliveries is invalid too, and `sendbench report`
  exits non-zero for an invalid run.
- Bucket-limited runs (`DEST_RATE` at most 300): `met` also needs the window rate at most
  `rate x (1 + tolerance 3%) + burst / window seconds` and the busiest second at most
  `rate x 1.03 + burst`; the report names the tolerance and the burst slack and says EXCEEDED
  otherwise.
- From each worker's `/metrics` (summed over processes; quantiles are estimated from the buckets):
  `zirv_flow_job_claim_lag_seconds{kind="delivery.send"}`, the render pool's
  `zirv_flow_render_wait_seconds`, `zirv_flow_render_duration_seconds` and
  `zirv_flow_render_restarts_total`, and the destination counters. **There is no dedicated
  rate-bucket wait metric in the product**; the bucket shows as `delivery.send` job duration and
  claim lag, the queue gauges and `messages.txt`, and the report says so rather than inventing a
  number.
- Queue gauges: `zirv_flow_destination_queue_depth` and `_oldest_seconds` are published by the
  scheduler leader, not the workers, so `run.sh` samples the scheduler (`:9101`) every second into
  `queue.csv`; the report gives the max depth and max oldest age inside the steady window (a queue
  standing there means the destination or the workers were the limit; `variant.txt` records it).
- Database CPU: `docker stats` of the PostgreSQL container every few seconds (about 6 s), cut to the steady window by
  the report (mean and max percent of one core, with the sample count).
- `messages.txt` holds the outcome counts of the flow's messages and any dead jobs.

Results are in `$M6_OUT/<label>/`. Run the light variants first (a few thousand people), then the
full set, and report them in `docs/benchmarks/m6-send.md` with the hardware and PostgreSQL settings
(`$M4_OUT/stack.txt` from `stack.sh up`).

## 12. Render benchmark (M7, issue #317)

`tools/renderbench` renders a typical block email (about 20 blocks, a theme with a footer snippet and a
logo, one snippet block, an asset image, three pre-evaluated conditions) through the real render pool
(`RenderPool::render_email_with`) with as many calls in flight as children, and times each call end to
end. It runs pool size 1 and the production default (`render.pool_size`, 2), 200 warm-up renders and
5000 measured per run (the warm-up is not in the percentiles), then the same render in-process. The
children are the tool's own executable run as `render-host`. It needs no database and no Docker, and
it is a separate tool, not a `loadgen` mode, because `loadgen` links no zirv-flow crate.

```sh
RENDER_BENCH_CONSTRAINT="session pinned to the 16 E-core threads (mask 0xFFFF0000)" \
  scripts/bench/m7/render.sh          # release build; writes docs/benchmarks/data/m7/render.json
```

The report is `docs/benchmarks/m7-render.md`. The smoke run (20 renders, the pool, a report) is a unit
test of the tool and runs in the normal test job.

## 13. M8 reference-resource emulation (issue #393)

For the operator preparing the section 19 validation suite: this is **reference-resource emulation,
not a reference-hardware run** (ruling ef99d178). Shared storage, Docker Desktop overhead and the
mixed core types of the dev CPU remain limitations. Validation on actual reference hardware stays
outstanding. Renting machines requires the operator's explicit spending authorisation; this change
provisions no machines. The build-only change has not run Compose, seeded a database or measured
a benchmark. A successful shell self-test is not a successful smoke run.

`m8/compose.m8.yaml` layers on `docker-compose.yml` and `compose.bench.yaml`, replacing the shared
app cpuset of `compose.tiers.yaml`. The existing CLI has separate `worker` and `scheduler` commands,
so two app **allocations** use three role containers: serve alone, worker+scheduler together. The
one-shot key generator exits before measurement. Defaults, all overridable:

| Variable | Default | Purpose |
|---|---|---|
| `M8_PG_CPUSET` | `8-15` | Eight logical CPUs for PostgreSQL 16.15 |
| `M8_SERVE_CPUSET` | `0-3` | Four logical CPUs for serve |
| `M8_WORKER_CPUSET` | `4-7` | Four logical CPUs shared by worker and scheduler |
| `M8_PG_MEMORY` / `M8_PG_SHM` | `32g` / `8g` | PostgreSQL memory cap / shared memory |
| `M8_SERVE_MEMORY` | `8g` | Serve cap |
| `M8_WORKER_MEMORY` / `M8_SCHEDULER_MEMORY` | `7g` / `1g` | Second app allocation's split caps |
| `M8_PG_IMAGE` | `postgres:16.15` | PostgreSQL image; used by compose and the topology check |
| `M8_SHARED_BUFFERS` | `8GB` | One quarter of the default 32 GiB PG budget |
| `BENCH_IMAGE_TAG` | `0.190.0` | Pre-pulled versioned app image; no `latest` or build fallback |

The other PostgreSQL settings match [M4 section 2.1](../../docs/benchmarks/m4-segments.md#21-hardware-spec-d1):
`effective_cache_size=16GB`, `maintenance_work_mem=1GB`, `max_wal_size=8GB`,
`checkpoint_timeout=15min`; 8 GiB shared memory follows the corrected M4 run. If changing the PG
cap, size `M8_SHARED_BUFFERS` with it. `up` rejects intersections among all three CPU allocations
before calling Docker. An enrollment seed also checks that the Docker VM can hold the sum of the
configured memory caps (48 GiB by default). These IDs are logical CPUs, not claims about identical
physical cores. Give the Docker VM at least 16 logical CPUs; the send sink defaults to CPU `16`,
requiring a seventeenth, outside the app/PG allocations (`M6_SINK_CPUSET` can override it).

Run from the checkout root on a dedicated bench host/CI runner. Runtime prerequisites are Bash,
Python 3, Docker Compose with `!reset`/`!override` support, jq, curl and GNU coreutils (M6 uses GNU
`date`). Debugger, jobs and timers also require host `psql` (jobs and timers also the per-kind sampler);
`smoke`, `all` and those benches check this before spending time, and `all` runs every bench,
lists failed ones in its summary and exits non-zero if any failed; the other helpers run it inside
PostgreSQL. Install CI-produced `loadgen`, `renderbench` and `sendbench`
executables at `target/release/`, or set `LOADGEN`, `RENDER_BENCH_BIN` and `SENDBENCH` to their paths.
For send, install the Linux `sendbench` artifact at `.zirv/work/m8/send-tools/linux/sendbench`.
Do not use M6's `sink.sh build`: M8 calls only `sink.sh start`, with `debian:bookworm-slim` as the
sink runtime. Pre-pull that image, `postgres:16.15` and the selected app image before running.

```sh
bash scripts/bench/m8/run.sh --self-test  # local, no Docker, database or compiled tools
# The following are run commands for an authorised bench runner, not build-only validation:
bash scripts/bench/m8/run.sh up
bash scripts/bench/m8/run.sh seed people
bash scripts/bench/m8/run.sh seed events
bash scripts/bench/m8/run.sh seed enrollments
bash scripts/bench/m8/run.sh all
# A separate small database and state, on the same published ports (stop the full stack first):
bash scripts/bench/m8/run.sh smoke
```

The fixed project is `zirv-flow-m8`, or `zirv-flow-m8-smoke`; the database is `zirv_flow_m8` with
throwaway local credentials. No caller-supplied database URL is used. The scripts never remove
volumes. State and credentials stay under `.zirv/work/m8` (`M8_STATE` overrides it); smoke appends
`-smoke`. Smoke uses 5,000 people, 5,000 events and 3,000 waiting enrollments across three workspaces,
50 ingest items/s for 60 seconds, two preview samples, one refresh, and 20 measured renders.
Debugger smoke uses one client and five seconds of load; jobs uses 3,000 busy jobs plus one quiet
job per scenario; timers uses a five-second wake window per scenario.

Seeds use SQL only in the bench database. They add no migration, product table or `DELETIONS`
entry. M4's generator retains the OQ-1/M4 mix of six strings, five numbers, four booleans, two
timestamps, one date and two lists. M5's staging schema and loader reuse its checked-in
product-path sample; the SQL remaps workspace/flow/person IDs and creates the fixture flow without
calling the API. The seed establishes waiting volume, not enrollment-throughput evidence.

| Seed | Default target (`M8_*`) | Bytes/row and source | Initial free-space requirement |
|---|---|---|---|
| people | `M8_PEOPLE=25000000`: 10 M / 10 M / 5 M | 2,048 B, rounded above M4's 1,270 B/person plus 339 B/identity, allowing auxiliary indexes/consent | 110,989,934,592 B (103.4 GiB) |
| events | `M8_EVENTS=500000000`: 200 M / 200 M / 100 M, 365 days in monthly partitions | 512 B, rounded above M4's (10.1 + 12.1) GB / 50 M = 444 B/event | 520,589,934,592 B (484.8 GiB) |
| enrollments | `M8_ENROLLMENTS=10000000`: 4 M / 4 M / 2 M, spread over 40 minutes | 4,096 B estimate for enrollment, step, job, entry, three decisions, staging and indexes; no measured M5 size claim | 90,509,934,592 B (84.3 GiB) |

The measured size source is [M4 section 2.2](../../docs/benchmarks/m4-segments.md#22-data-spec-d3).
Each estimate is `remaining rows × bytes/row × 2 + 8 GiB`: the factor allows WAL, temporary work
and index growth; the reserve is additional headroom. It is a conservative admission estimate,
not a measured peak or a guarantee against later disk exhaustion. Free space comes from `df -Pk`
inside PostgreSQL on its Docker data volume, never the checkout filesystem. A shortage refuses
before writes. Plan the full event seed as an overnight job; #393's build-only work has no timings.

Resume with the same sizes: `m8bench.config` fixes the sizes and event anchor, and refuses a changed
scale. Each `M8_CHUNK` batch (default 100,000 rows) atomically advances
`m8bench.progress(kind, workspace_id).last_n` with the inserted rows; a failed batch rolls back,
and a rerun starts after the last committed row. Each seed finishes with M4/M5's
`VACUUM (ANALYZE, PARALLEL 0)` on its tables; a rerun also retries this maintenance.
People/events use M4's deterministic workspace/row
IDs. Enrollments retain M5's `(run=1, set=workspace UUID, n)` staging keys. The waiting window starts
seven days after initial seed configuration, allowing overnight loads; `timers` deliberately
re-arms that window before each measurement. A new enrollment batch refuses a window less than
one hour away. Completed/consumed rows are never re-created by
resuming. Do not change or erase seed rows manually. Events and enrollments require the people
seed to finish first; all three targets must be supplied together when selecting a smaller scale.

| Command | Existing measurement / registration |
|---|---|
| `ingest` | `tools/loadgen` track workload on the running stack |
| `segments` | `m4/measure.sh all`, including the incremental load window |
| `send` | M6 sink/setup and `m6/run.sh`; SQL email/consent fixtures on existing M8 people |
| `render` | `m7/render.sh` with prebuilt `tools/renderbench`; **native host tool, outside Docker CPU/memory caps**, explicitly labelled in JSON |
| `debugger` | Watcher receipts and host SQL latency/clock report, alongside track load |
| `explorer` | Non-zero until #395 registers it |
| `jobs` | SQL no-op throughput and busy/quiet workspace fairness, with and without a cap |
| `timers` | M5 lateness query on the M8 waiting seed, idle and alongside track ingest |
| `decisions` | Non-zero until #397 adapts `ab.sh`, which currently deletes its own fixed stack and launches unpinned Windows processes |

`all` and `smoke` note and skip only unregistered commands; a registered benchmark failure stops
the suite. Bench setup may use the existing authenticated APIs; scale seeds do not. Each completed
measurement writes `docs/benchmarks/data/m8/<bench>.json` (`smoke` writes
`docs/benchmarks/data/m8/smoke/<bench>.json`; a failed run writes `<bench>.failed.json` beside it
and never replaces the last good file), including the image version, actual
container images/cpusets/core counts/memory limits, host memory, Docker VM CPU/memory from
`docker info` (null when unavailable), the effective PostgreSQL settings, committed seed sizes, argument arrays, exit status, raw JSON
results, the emulation label and all three limitations. Later good runs replace that bench's summary;
raw runs remain in the state directory. No measured data files are fabricated by this change.

Run `bash scripts/bench/m8/check-report.sh` to validate `docs/benchmarks/m8-validation.md`
(`--self-test` tests temporary fixtures; `--data-only` checks just JSON). Its Markdown table needs
`Dimension`, `Data file` and `Verdict` columns: exact section 19 dimension names, existing M8 JSON
citations (links relative to the report or literal paths starting at `docs/`; never smoke or `.failed.json` files, and the run needs `smoke` false and `exit_code` 0), and exactly `met`,
`missed and fixed`, or `revised by decision`. The report must contain the one-line label
"reference-resource emulation, not a reference-hardware run". Every JSON under
`docs/benchmarks/data/m8/`, including smoke and failed runs, must parse and carry a nonempty
`image_version` string and a `topology` object. Invalid input exits 1; absent reports are skipped
and zero data files pass. CI always checks fixtures and data, and checks the report once present.

`debugger` starts `M8_DEBUGGER_CLIENTS=1` streams (set **20** for the full run), waits for all to
connect, then offers track load at `M8_RATE=2000` for `M8_SECONDS=600`. Connection allowance is
`M8_DEBUGGER_CONNECT_SECONDS=15`; drain allowance is `M8_DRAIN_SECONDS=60` (both five in smoke).
It awaits normal watcher completion to flush receipts, scrapes debugger metrics, and runs the
clock-gated SQL with host `psql` on `127.0.0.1:55432`, database/user `zirv_flow_m8`/`zirv`, password
`zirv-m8-bench` fixed by the M8 compose layer. Missing host `psql`, connection/drain timeout or a
clock gate fails the run. Raw `debugger.json` includes counts, p50/p95/p99/max, delivered share,
gap frames, skipped items and clock offset; `receive.jsonl` retains the receipts.

`timers` requires a completed `seed enrollments`. With the worker stopped it restores the M5
waiting-row templates and re-arms the M8 seed for `noload/` then `loaded/`; the latter adds track
ingest at `M8_RATE`. `M8_TIMERS_SECONDS=2400` preserves the seed's forty-minute spread,
`M8_TIMERS_LEAD_SECONDS=600` allows for re-arming, and `M8_DRAIN_SECONDS=3600` bounds the drain
(smoke: 5/10/60 seconds). A re-arm that overruns its lead rolls back; after the worker is ready
the earliest pending wake must still lie ahead (else the run fails), and `timers.json` records the
lead left then as `lead_left_at_worker_ready_s`. Each raw scenario holds
M5 `lateness.json`, `timers.json`, the timer-wake histogram and per-kind job samples. Sets are
workspace UUIDs, with no M5 burst set. The worker restarts between scenarios, resetting its metrics.

`jobs` queues `M8_JOBS_BUSY=100000` ready `maintenance.noop` jobs in workspace 1 ahead of one in
workspace 2, first uncapped then with `M8_JOBS_CAP=4` (use `8` to compare another cap, or `unset`
for only uncapped). It sets the worker concurrency itself with `M8_JOBS_CONCURRENCY=16` (no-op jobs
are cheap; the topology default of 4 would never let a cap of 4 bind) through the same compose
override as the cap, records the effective value in `worker.json`, and refuses a cap at or above
the concurrency. The worker is restored to the topology default afterwards. Leftover no-op rows of
a run are deleted on every exit path. `M8_JOBS_LEAD_SECONDS=10` allows sampler startup; `M8_JOBS_TIMEOUT_SECONDS=600`
bounds completion. Raw `cap-*/jobs.json` reports completions/s against 1,000/s, exact quiet claim
lag from a temporary bench trigger, and the maximum sampled busy running count (fails above the
cap). `M8_JOBS_TICK_SECONDS=1` is the sleep between polls (a 100,000-job run lasts about 100 s, so the
final polling delay is at most about 1%; smoke's 3,000 jobs are not a measurement); throughput
includes it and samples cannot prove an unsampled peak. Observer effects: the ready-count poll and
the sampler's `run_at <= now()` query run during the window and may scan the `run_at` index while
future wakes are pending; both are named in the `measurement` field of the jobs and timers reports. The trigger is removed and the base worker restored.

Jobs and timers call `m8/sample-job-kinds.sh` and `job-kinds.sql` from #455; merge that dependency
and select an image emitting its per-kind claim metrics before running. `JOB_KINDS_TICK_S=5`
controls that sampler. CSV samples and their JSON copy remain under each raw scenario, alongside
Prometheus scrapes and workload results; `write_data` embeds all raw JSON in the M8 report.

The no-Docker self-test is in the existing CI job beside the benchmark feature gate. The feature
gate rejects benchmark paths in Dockerfiles/CD inputs; `.dockerignore` excludes benchmark scripts
and data from the build context. The full gate still invokes Cargo and is run only by CI when
local builds are forbidden.

## 14. Per-kind job sampling (M8, issues #207 and #396)

While a run is active, use `scripts/bench/m8/sample-job-kinds.sh` with psql's `PG*` connection
variables and every worker's metrics URL once (default `http://127.0.0.1:9100/metrics`):

```sh
PGHOST=127.0.0.1 PGPORT=55432 PGUSER=zirv PGDATABASE=zirv_flow_m8 \
  scripts/bench/m8/sample-job-kinds.sh http://127.0.0.1:9100/metrics >job-kinds.csv
```

It needs psql, curl, awk and jq; stop with Ctrl-C. Each sample emits one CSV row per observed kind:
`t`, `kind`, `claims_total`, `completed_total`, `claim_lag_seconds_sum`, `due_ready`, `running`,
`ready_lag_mean_seconds`, `ready_lag_max_seconds`. Worker metrics supply claims and lag from
`zirv_flow_job_claim_lag_seconds_count`/`_sum` (claim time minus `run_at`) and completions from
`zirv_flow_jobs_total{outcome="completed"}`; SQL supplies due/running counts and ready lag
(database `now()` minus `run_at`, blank when none are due). Completed jobs are deleted, so SQL
cannot count their completions. Counters sum all supplied workers and cover their lifetimes:
take deltas for interval counts and divide the lag-sum delta by the claim-count delta for mean
claim lag (undefined with no claims). Start a new file if workers restart. Samples cover all
workspaces; SQL and worker scrapes are sequential, not atomic. `JOB_KINDS_TICK_S` sets the sleep
between samples (default 5 seconds); a query or scrape failure stops the sampler.
