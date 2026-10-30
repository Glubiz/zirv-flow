# Runtime operations

## Logs and traces

The long-running commands (`serve`, `worker`, `scheduler`, `all`) write
structured logs to standard error, as one JSON object per line by default
(`log.format = "text"` for development, `log.filter` to choose levels). One-shot
commands (`migrate`, `config check`, `admin`) print plain text.

Every HTTP request runs in an `http.request` span with `request_id`, `trace_id`,
`method` and the matched `route`, and ends with one `request completed` event
with `status` and `duration_ms` (at `debug` for `/healthz`, `/readyz` and
`/metrics`). Every job runs in a `job` span with `job_id`, `kind`, `workspace_id`,
`attempt`, `trace_id`, `cause_kind` and `cause_id`; payloads are never logged.

The server mints a fresh trace id for each request and returns it in the
`x-trace-id` response header and in the `trace_id` of problem bodies; an incoming
trace header is ignored. Jobs enqueued by the request, and by the jobs that
follow, carry the same trace id. Failed attempts and lease expiries of workspace
jobs are recorded in the decision log, which
`GET /api/v1/workspaces/{ws}/traces/{trace_id}` returns for a member or a `read`
API key (400 for a malformed id, 404 when the workspace has no records for it).

### Tracing export

Set `log.otlp.endpoint` (`ZIRV_FLOW_LOG__OTLP__ENDPOINT`) to send the
`http.request` and `job` spans, and the spans inside them, to an OTLP collector
over HTTP/protobuf (spans go to `<endpoint>/v1/traces`). Exported spans use the
zirv trace id as their trace id, so a trace in the collector matches
`x-trace-id`, the logged `trace_id` and the decision log, and a request and the
jobs it enqueued share one trace. A valid W3C `traceparent` on a request is
attached to its span as a link and never becomes its parent or changes the
trace id. Without the setting nothing is created or sent. Export failures never
fail a request or job; spans are batched, a full queue drops spans, and the
process flushes what is queued (up to 5 seconds) when it shuts down. The log
output is the same with or without export. `log.filter` also decides which
spans are exported, so a filter such as `warn` exports none.

## Metrics

`GET /metrics` serves Prometheus text with the `zirv_flow_` prefix. Label values come from
closed sets (a route template, a kind, an outcome, a feature) or are ids; no email, phone, name,
property value, key, prompt or model output is ever a label. The queue, enrollment, freshness,
destination and dirty-set gauges are sampled by the scheduler leader, so they appear where the
leader runs. The enrollment gauges read the live rows through `enrollments_live_idx` under a 5 second
statement timeout; at 11 million live rows the count takes about half a second and the overdue-timer
lookup well under a millisecond. A test fails when a `zirv_flow_` series is emitted in `crates/` and missing from this
table.

Example Prometheus alert rules are in [`docs/operations/alerts.rules.yml`](alerts.rules.yml),
with one runbook per alert in [`docs/operations/runbooks.md`](runbooks.md). The
thresholds are starting points. A unit test fails when a rule names a series or label the code
does not emit, an alert lacks its runbook section, or a runbook names a configuration key that
does not exist.

| Series | Type | Labels | What it measures |
|--------|------|--------|------------------|
| `zirv_flow_http_requests_total` | counter | `method`, `route`, `status` | HTTP requests by matched route template |
| `zirv_flow_http_request_duration_seconds` | histogram | `method`, `route` | HTTP request latency |
| `zirv_flow_db_pool_connections` | gauge | `pool`, `state` | Pool connections by state |
| `zirv_flow_db_pool_max_connections` | gauge | `pool` | Pool size limit |
| `zirv_flow_jobs` | gauge | `kind`, `state` | Queue depth for `ready`, `running` and `dead` |
| `zirv_flow_jobs_total` | counter | `kind`, `outcome` | Job attempts by how they ended |
| `zirv_flow_job_duration_seconds` | histogram | `kind` | Handler duration |
| `zirv_flow_job_claim_lag_seconds` | histogram | `kind` | Time from `run_at` to the claim |
| `zirv_flow_retention_deleted_total` | counter | `table` | Rows a retention job removed; `table` is `messages`, `decision_records`, `jobs` or `audit_log` |
| `zirv_flow_retention_last_run_timestamp_seconds` | gauge | `table` | Unix time of the last retention run of `table` |
| `zirv_flow_egress_denied_total` | counter | `class` | Outbound requests refused by the egress policy |
| `zirv_flow_ingest_requests_total` | counter | `endpoint`, `status` | Ingestion requests |
| `zirv_flow_ingest_items_total` | counter | `kind` | Items accepted into the inbox |
| `zirv_flow_ingest_applied_total` | counter | `kind`, `outcome` | Inbox items applied, by validation outcome |
| `zirv_flow_ingest_late_total` | counter | none | Events that arrived late |
| `zirv_flow_ingest_inbox_age_seconds` | gauge | none | Age of the oldest unprocessed inbox row |
| `zirv_flow_webhook_requests_total` | counter | `outcome` | Inbound webhook requests |
| `zirv_flow_debugger_clients` | gauge | none | Open debugger streams on this process |
| `zirv_flow_debugger_delivery_seconds` | histogram | none | Server side of debugger latency: from the feed row's `at` to the frame write. The end-to-end figure counts from the inbox append and is measured from `events.received_at` |
| `zirv_flow_explorer_query_seconds` | histogram | `kind` | Explorer query duration; `kind` is `count`, `breakdown`, `list` or `export` |
| `zirv_flow_explorer_timeouts_total` | counter | `kind` | Explorer queries that ran past their statement timeout (503 `query_timeout`) |
| `zirv_flow_explorer_exports_total` | counter | none | Explorer exports started |
| `zirv_flow_decision_append_seconds` | histogram | none | Time of one decision-log append call (a batch is one observation); records nothing in a `bench-no-decisions` build |
| `zirv_flow_decision_log_bytes` | gauge | none | Total size of the decision-log partitions, refreshed by the `decisions.retention` job |
| `zirv_flow_decision_log_partitions` | gauge | none | Decision-log partitions, the default one included |
| `zirv_flow_sync_runs_total` | counter | `outcome` | Ended sync runs |
| `zirv_flow_sync_run_duration_seconds` | histogram | none | Sync run duration |
| `zirv_flow_sync_records_total` | counter | `outcome` | Records of committed sync batches |
| `zirv_flow_sync_events_total` | counter | `outcome` | Events of committed sync batches |
| `zirv_flow_stream_freshness_seconds` | gauge | `workspace`, `stream` | Age of a stream's data |
| `zirv_flow_stream_state` | gauge | `workspace`, `stream` | 0 fresh, 1 stale, 2 failing |
| `zirv_flow_streams` | gauge | `workspace`, `state` | Enabled streams in each state |
| `zirv_flow_stream_lag_messages` | gauge | `workspace`, `stream` | Messages behind, for consumer streams |
| `zirv_flow_source_requests_total` | counter | `source_type`, `outcome` | Source HTTP requests |
| `zirv_flow_source_retries_total` | counter | `source_type`, `reason` | Source request retries |
| `zirv_flow_source_oauth_total` | counter | `outcome` | Source token fetches |
| `zirv_flow_source_rate_limit_wait_seconds_total` | gauge | `source_type` | Seconds spent waiting on a source's rate limit |
| `zirv_flow_source_db_batches_total` | counter | `source_type` | Database source batches |
| `zirv_flow_source_db_errors_total` | counter | `source_type`, `reason` | Database source errors |
| `zirv_flow_kafka_fetches_total` | counter | `outcome` | Kafka fetches |
| `zirv_flow_kafka_offset_resets_total` | counter | none | Kafka offset resets |
| `zirv_flow_segment_refresh_seconds` | histogram | none | Duration of one `segment.refresh` |
| `zirv_flow_segment_refresh_people` | histogram | none | People who entered or left the segment in one refresh |
| `zirv_flow_segment_dirty_backlog` | gauge | none | People marked dirty, over all workspaces |
| `zirv_flow_segment_dirty_age_seconds` | gauge | none | Age of the oldest dirty mark |
| `zirv_flow_flow_enrollments_total` | counter | `trigger`, `outcome` | People a `flow.enroll` enrolled or found enrolled |
| `zirv_flow_flow_enroll_skips_total` | counter | `trigger`, `reason` | Enrollments refused, by reason |
| `zirv_flow_enrollments` | gauge | `state` | Enrollments that are `active` or `waiting` |
| `zirv_flow_enrollments_last_sample_timestamp_seconds` | gauge | none | Unix time of the last successful enrollment sample; alert on it going stale, since a failed sample leaves the gauges at their last values |
| `zirv_flow_timer_lateness_seconds` | gauge | none | Age of the oldest `waiting` enrollment whose `wake_at` has passed; 0 with none |
| `zirv_flow_timer_wake_lateness_seconds` | histogram | none | Time between a timer's `wake_at` and its `flow.step` running |
| `zirv_flow_destination_queue_depth` | gauge | `workspace`, `destination` | Items waiting for delivery |
| `zirv_flow_destination_queue_oldest_seconds` | gauge | `workspace`, `destination` | Age of the oldest waiting item |
| `zirv_flow_destination_attempts_total` | counter | `workspace`, `destination` | Delivery attempts |
| `zirv_flow_destination_outcomes_total` | counter | `workspace`, `destination`, `outcome` | Delivery attempts by outcome |
| `zirv_flow_delivery_attempt_record_failures_total` | counter | `workspace` | Attempts whose record could not be written |
| `zirv_flow_destination_forwarding_depth` | gauge | `workspace`, `destination` | Forwarding outbox rows waiting |
| `zirv_flow_destination_forwarding_lag_seconds` | gauge | `workspace`, `destination` | How long the oldest outbox row has been past due |
| `zirv_flow_destination_forwarding_dead_total` | counter | none | Outbox rows settled as dead letters |
| `zirv_flow_provider_feedback_total` | counter | `outcome` | Provider feedback received |
| `zirv_flow_render_duration_seconds` | histogram | `channel` | Template render duration |
| `zirv_flow_render_wait_seconds` | histogram | none | Wait for a render host |
| `zirv_flow_render_failures_total` | counter | `reason` | Failed renders |
| `zirv_flow_render_restarts_total` | counter | none | Render host restarts |
| `zirv_flow_ai_calls_total` | counter | `feature`, `outcome` | Assistant calls by feature and outcome |
| `zirv_flow_ai_refusals_total` | counter | `reason` | Calls refused before the provider, including `budget_exceeded` |
| `zirv_flow_ai_tokens_total` | counter | `feature`, `direction` | Tokens used; `direction` is `input` or `output` |
| `zirv_flow_ai_estimated_cost_micros_total` | counter | `feature` | Estimated cost of finished calls, in micros |
| `zirv_flow_ai_latency_seconds` | histogram | `feature` | Provider call latency |
| `zirv_flow_ai_provider_faults_total` | counter | `feature`, `reason` | Provider replies outside the reserved bounds |
| `zirv_flow_ai_draft_runs_total` | counter | `kind`, `outcome` | Draft runs |
| `zirv_flow_ai_drafts_accepted_total` | counter | `kind` | Drafts a person accepted |

## HTTP server

`zirv-flow serve` (and `all`) runs the startup database check, binds
`server.bind` and serves until Ctrl-C or SIGTERM, draining in-flight requests.

| Endpoint | Behaviour |
|----------|-----------|
| `GET /healthz` | Liveness; always 200 and never touches the database. |
| `GET /readyz` | 200 when the database is reachable and its schema is up to date, otherwise 503. |
| `GET /metrics` | Prometheus text, including `zirv_flow_http_requests_total`, `zirv_flow_http_request_duration_seconds` and `zirv_flow_db_pool_connections{pool,state}`. Served on `server.bind` for `serve`/`all`, or on `ops.bind` when configured for `worker`/`scheduler`. |
| `GET /api/v1/version` | The running version. |

Errors are `application/problem+json` (RFC 9457). Every response carries an
`x-trace-id` and an
`x-request-id`; a valid incoming one (1 to 128 characters of `A-Za-z0-9._-`) is
kept, otherwise one is generated.

The OpenAPI 3.1 document is committed at `crates/zirv-flow-api/openapi.json`,
and a test fails when it drifts from the code. After changing an API
annotation, regenerate it with:

```sh
ZIRV_FLOW_BLESS=1 cargo nextest run -p zirv-flow-api openapi
```

## Background jobs

Background work runs through a durable job queue in PostgreSQL. Three roles
use it:

| Command | What it does |
|---------|--------------|
| `zirv-flow worker` | Claims and runs jobs, `worker.concurrency` at a time. Run as many as you like. |
| `zirv-flow scheduler` | Elected leader (an advisory lock; the others stand by and take over within seconds): enqueues due cron schedules, returns jobs with expired leases to the queue, keeps the decision log and event partitions for this and next month, expires person exports hourly, deletes import files and rejects past their retention (see [CSV import](../guide/sources.md#csv-import)), and exports queue gauges. |
| `zirv-flow all` | `serve`, `worker` and `scheduler` in one process (the default). |

A running job holds a lease of `worker.lease_seconds`, which its worker extends
every third of that while the handler runs. A worker that dies leaves its job to
the scheduler, which makes it runnable again once the lease has expired; a job
that fails is retried with exponential backoff and jitter, and is kept as
`dead` after its last attempt. A job's database writes commit together with its
completion, so a job never completes twice. Work that reaches outside the
database must be idempotent: a crash can run a job again. A workspace is limited
to `worker.max_running_per_workspace` running jobs when that is set, so one busy
workspace cannot take every worker.

For pool sizes and the deployment connection budget, see [connection arithmetic](sizing.md#connection-arithmetic).

On `SIGTERM` or Ctrl-C a worker stops claiming and has 30 seconds to finish
running jobs; any still running are left to lease expiry. The
`ingest.drain` kind (one per workspace) applies that workspace's ingestion inbox; see
[Ingestion](../people.md#ingestion). `/metrics` carries
`zirv_flow_jobs_total{kind,outcome}`, `zirv_flow_job_duration_seconds{kind}`,
`zirv_flow_job_claim_lag_seconds{kind}` and the queue depth
`zirv_flow_jobs{kind,state}` (`ready`, `running`, `dead`), set by the scheduler
leader. A `worker` or `scheduler` process has no HTTP server, so set `ops.bind` (for
example `ZIRV_FLOW_OPS__BIND=0.0.0.0:9100`) to serve `/metrics`, `/healthz` and
`/readyz` from it; `/readyz` is 503 once a role loop has stopped or shutdown has
begun. With `serve` or `all` the same endpoints are on `server.bind` and
`ops.bind` is ignored.

## Render host

Templates render in child processes, never inside the API or a worker, so a template
that allocates without bound fails its own render and nothing else. `serve` and `worker`
(and so `all`) start `render.pool_size` children at boot, each a copy of this executable
running the hidden `render-host` command, which an operator never runs by hand. A render
takes a free child, and a child that overruns `render.deadline_ms`, dies, or runs out of
memory is killed and replaced; only that render fails, with one of the reasons
`memory_limit`, `deadline`, `child_exited` or `protocol`.

On Linux each child limits its own address space to `render.memory_limit_mib` before it
reads its first request. Raise it if a legitimate render fails with `memory_limit`; the
loaded binary counts against it as well as the heap, so values under 64 are refused.
Other operating systems have no such limit: the child logs one warning at startup and
runs unlimited, which is for development only. Memory a child uses is outside the
worker's heap, so size the container for `render.pool_size` extra processes per role.

Metrics: `zirv_flow_render_duration_seconds{channel}`, `zirv_flow_render_wait_seconds`,
`zirv_flow_render_restarts_total` and `zirv_flow_render_failures_total{reason}`.

## Import benchmark

To time a large file, run the ignored test against a PostgreSQL instance with
`ZIRV_FLOW_BENCH_ROWS=1000000 cargo nextest run -p zirv-flow-jobs a_big_file --run-ignored all --no-capture`;
it prints the duration and the rows per second.

