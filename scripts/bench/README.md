# Benchmark runbook

How to run the ingestion benchmarks of issue #47 (architecture section 19 targets and OQ-1). The
tools here only measure; the results, the environment and the pass or miss calls go in
`docs/benchmarks/m2-ingestion.md`, and every raw report JSON in `docs/benchmarks/data/`.

| Tool | What it is |
|------|------------|
| `tools/loadgen` | `loadgen`, a paced, seeded load generator that writes a JSON report |
| `scripts/bench/applied-latency.sql` | inbox-to-applied p50, p95 and p99 for track and identify, from the database |
| `scripts/bench/compose.bench.yaml` | compose override: separate serve, worker and scheduler, PostgreSQL published on 55432 |
| `bench-no-decisions` | cargo feature of `zirv-flow-store`: decision appends become no-ops (benchmark builds only) |
| `scripts/bench/oq1/` | the OQ-1 trait storage benchmark (SQL and `run.sh`) |

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

## 2. Create the `ingest` API key

There is no CLI for API keys; they are created over the API by a signed-in workspace admin (README,
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
  is how late requests were sent; report them beside it, never instead of it. All cover 202s.
- A 429 sets one shared pause (the largest `Retry-After` seen, 1 to 60 s). The refused requests go
  back to the dispatcher, which resends them after the pause, ahead of new ones and paced at the
  target rate (up to 8 sends each); the pause is not made up afterwards, so there is no stampede.
- Payloads depend on `--seed` and the item number alone. Every `messageId` is `lg-<run-id>-<n>`;
  `--run-id` defaults to the start time, so a rerun does not dedupe against an earlier run (pass
  the same one to repeat a run exactly). Items carry no `timestamp`: the server stamps arrival,
  which `applied-latency.sql` relies on for identify.
- The report has `total` and one entry per phase with `achieved_rate`, `status`
  (`accepted_202`, `rate_limited_429`, `server_5xx`, `other`, `transport_errors`), `retries_after_429`,
  `late_dispatch` and the three latency blocks above (`p50_ms`, `p95_ms`, `p99_ms`, `max_ms`). Only plain `http://` is supported.

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

The feature is compile-time only. Build two binaries and run them natively against the compose
PostgreSQL (published on 55432), not through the image, which never carries the feature:

```sh
cargo build --release -p zirv-flow && cp target/release/zirv-flow ./zirv-flow-on
cargo build --release -p zirv-flow --features zirv-flow-store/bench-no-decisions \
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
