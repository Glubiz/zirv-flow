# Runbooks

One section per alert in [`alerts.rules.yml`](alerts.rules.yml), in the same order. Each alert
links to its section by the lowercase name, for example `#zirvflowjobsbacklog`. Every section has
the same seven parts: what it means, the user-visible effect, how to confirm, the likely causes in
order, the fix, how to know it is fixed, and when to escalate.

Conventions used below:

- `$BASE` is the URL of a `serve` or `all` process (`server.bind`). A `worker` or `scheduler`
  process serves `/metrics`, `/healthz` and `/readyz` only on `ops.bind`.
- Admin endpoints need the instance owner's session; use the web app signed in as the owner. No
  step here needs, prints or pastes a secret, an address or a person's name.
- `psql "$DATABASE_URL"` is a read-only probe against the database. Every probe counts or groups;
  none selects a person, an address or a payload.
- The metric names are in the README metric table. The config keys are in the README
  Configuration table; `zirv-flow config check` prints the effective values with secrets redacted.

## ZirvFlowJobsBacklog

### What it means

More than 10000 jobs of one kind have been `ready` for 15 minutes: workers are not keeping up
with what is queued.

### User-visible effect

Work of that kind is late. For `ingest.drain` new events take longer to appear; for `flow.step`
flows run late; for `segment.refresh` segments are stale; for sends, messages go out late.

### Confirm

1. Metric: `sum by (kind) (zirv_flow_jobs{state="ready"})`, and the same with `state="running"`.
2. As the owner, `GET $BASE/api/v1/admin/jobs` gives `ready`, `running`, `dead` and
   `oldest_ready_seconds` per kind.
3. Probe:
   `psql "$DATABASE_URL" -c "SELECT kind, state, count(*) FROM jobs GROUP BY 1, 2 ORDER BY 3 DESC LIMIT 10"`.
4. Count the worker processes that are up: the `zirv_flow_jobs_total` counters should be rising.

### Likely causes

1. No worker is running, or all are down (a deploy, a crash loop).
2. Workers are saturated: `worker.concurrency` is too low for the arrival rate.
3. One workspace floods the queue and `worker.max_running_per_workspace` is unset.
4. A handler is slow or stuck: `zirv_flow_job_duration_seconds` for the kind has grown.
5. The database is slow or its pool is full (see ZirvFlowDbPoolSaturated).

### Fix

1. Start the missing workers: `zirv-flow worker` (or `zirv-flow all`). Check `GET /readyz` on
   `ops.bind`.
2. Add worker processes, or raise `worker.concurrency` and restart. Each worker adds
   `(2 + ingest.drain_shards) * worker.concurrency + 1` database connections, so check the
   database connection limit first.
3. Set `worker.max_running_per_workspace` so one workspace cannot take every worker, and restart.
4. For a stuck handler, read the worker logs for the kind and fix its cause; a job whose lease
   expires returns to the queue by itself after `worker.lease_seconds`.

### Fixed when

`zirv_flow_jobs{state="ready"}` for the kind falls and `oldest_ready_seconds` for the kind in
`GET /api/v1/admin/jobs` drops under a minute. The alert resolves after the `for` period.

### Escalate

Escalate to the engineer on call when the backlog still grows after the workers are up and the
database is healthy, or when a handler fails on every attempt for no reason you can find in the
logs.

## ZirvFlowJobsDead

### What it means

Jobs of one kind used up their attempts and are kept as `dead`. Nothing runs them again unless
someone retries them.

### User-visible effect

The work behind each dead job did not happen: a message not sent, an import not run, an erasure
or export not finished. Which one depends on the kind.

### Confirm

1. Metric: `sum by (kind) (zirv_flow_jobs{state="dead"})`.
2. As the owner, `GET $BASE/api/v1/admin/jobs` for the counts per kind, then
   `GET $BASE/api/v1/admin/jobs/dead?limit=50` for the dead jobs: id, kind, attempts and the last
   error code. It never returns a payload.
3. Search the worker logs for the kind around the first failure time.

### Likely causes

1. An outage of something the handler calls (database, a destination, DNS) outlasted the retries.
2. A bad deploy: the handler fails on every job.
3. A job whose input is bad (a deleted record, a rejected file) and will never succeed.

### Fix

1. Fix the cause first. Retrying into the same failure only uses up the attempts again.
2. Retry one job with `POST $BASE/api/v1/admin/jobs/{id}/retry` (owner session with CSRF token)
   once the cause is fixed. It answers 409 when an equivalent job is already queued, and 404 when
   the job is not dead.
3. Leave a job whose input is bad. `jobs.retention` removes dead jobs after
   `retention.job_history_days`.

### Fixed when

The `dead` count for the kind stops rising and falls to 0 once the retried jobs complete or the
old ones are removed by retention.

### Escalate

Escalate when jobs of a kind keep dying after a fix, or when the kind is `destination.erase`,
`privacy.erase` or `privacy.export`, whose failure is a privacy obligation that is not met.

## ZirvFlowJobClaimLag

### What it means

The 99th percentile of the time between a job becoming due and a worker claiming it is over a
minute for a kind, for 10 minutes.

### User-visible effect

Timed work starts late. It is the early sign of ZirvFlowJobsBacklog and shows when the queue is
short but workers are starved or slow to wake.

### Confirm

1. Metric: `histogram_quantile(0.99, sum by (le, kind) (rate(zirv_flow_job_claim_lag_seconds_bucket[10m])))`.
2. `GET $BASE/api/v1/admin/jobs`: `oldest_ready_seconds` for the kind.
3. Compare `sum(zirv_flow_jobs{state="running"})` with the workers times `worker.concurrency`:
   when they are equal, every slot is busy.

### Likely causes

1. Every worker slot is busy with long jobs (`zirv_flow_job_duration_seconds`).
2. Too few workers, or `worker.concurrency` is too low.
3. `worker.max_running_per_workspace` holds back a busy workspace on purpose.
4. The database is slow to answer claims.

### Fix

1. Add workers or raise `worker.concurrency`, as under ZirvFlowJobsBacklog.
2. If one kind monopolizes the slots, find why its jobs are slow before adding capacity.
3. If the limit holds back a busy workspace on purpose, no fix is needed; raise the alert
   threshold for that deployment.

### Fixed when

The quantile falls under 60 seconds for the kind and `oldest_ready_seconds` stays low.

### Escalate

Escalate when claim lag stays high with idle workers: the claim query or the database is the
problem.

## ZirvFlowTimerLate

### What it means

`zirv_flow_timer_lateness_seconds`, the age of the oldest waiting enrollment whose `wake_at` has
passed, is over 5 minutes for 10 minutes. The scheduler leader samples it; 0 means none is late.

### User-visible effect

People stay in a flow's wait step longer than the flow says, so the next message or action runs
late.

### Confirm

1. Metric: `zirv_flow_timer_lateness_seconds`. Check that
   `time() - zirv_flow_enrollments_last_sample_timestamp_seconds` is small: a stale sample means
   the gauge is not trustworthy and the scheduler is the problem.
2. Probe:
   `psql "$DATABASE_URL" -c "SELECT count(*), EXTRACT(EPOCH FROM now() - min(wake_at))::bigint AS oldest_seconds FROM enrollments WHERE state = 'waiting' AND wake_at <= now()"`.
3. `GET $BASE/api/v1/admin/jobs`: the `flow.step` kind should not have a large `ready` count.

### Likely causes

1. The `flow.step` queue is backlogged (see ZirvFlowJobsBacklog).
2. `flow.step` jobs die (see ZirvFlowJobsDead).
3. The scheduler is not running or has lost leadership; the log says
   `the scheduler lost its leadership lock` or `scheduler tick failed`.

### Fix

1. Clear the queue or dead jobs of kind `flow.step` as in the two runbooks above.
2. Start `zirv-flow scheduler` (or `zirv-flow all`); one instance takes the leader lock within
   seconds.

### Fixed when

`zirv_flow_timer_lateness_seconds` returns to 0 and the probe returns no overdue rows.

### Escalate

Escalate when waiting enrollments stay overdue with a healthy queue and a leader: the wake
scheduling itself is broken.

## ZirvFlowInboxAge

### What it means

The oldest unprocessed row in the ingestion inbox is over 5 minutes old for 10 minutes.

### User-visible effect

Events and profile updates the API accepted do not show on people, in segments or in the debugger
yet. Nothing is lost: the rows stay in the inbox until applied.

### Confirm

1. Metric: `zirv_flow_ingest_inbox_age_seconds`, and `rate(zirv_flow_ingest_applied_total[5m])`
   against `rate(zirv_flow_ingest_items_total[5m])`.
2. Probe: `psql "$DATABASE_URL" -c "SELECT count(*) FROM ingest_inbox"`.
3. `GET $BASE/api/v1/admin/jobs`: the `ingest.drain` kind, ready, running and dead.

### Likely causes

1. No worker runs `ingest.drain`, or the workers are saturated (see ZirvFlowJobsBacklog).
2. Arrival is above what `ingest.drain_shards` and `ingest.drain_batch_size` can apply.
3. `ingest.drain` jobs are dead (see ZirvFlowJobsDead).
4. The database is slow.

### Fix

1. Get workers running and keep them healthy.
2. Raise `ingest.drain_shards` (1 to 16) for more concurrent drain loops per workspace, or
   `ingest.drain_batch_size`, and restart the workers. More shards need more connections per
   worker.
3. Retry dead `ingest.drain` jobs once the cause is fixed.

### Fixed when

`zirv_flow_ingest_inbox_age_seconds` falls under a minute and the inbox row count drops.

### Escalate

Escalate when the inbox grows although drain jobs run and succeed: the drain is slower than the
arrival rate and capacity needs a review.

## ZirvFlowStreamStale

### What it means

A stream is stale (state 1) or failing (state 2), and its data is over an hour old, for 15
minutes. The state is derived from the last run: failing means the last run failed; stale means
the time since the last success passed the stream's threshold.

### User-visible effect

Segments and flows that read the stream use old data. A flow that reads a stale input records the
freshness on its external effects.

### Confirm

1. Metrics: `zirv_flow_stream_state` and `zirv_flow_stream_freshness_seconds` (the labels are
   ids), and `zirv_flow_streams{state}` for the count per workspace.
2. In the web app open the source and read the stream badge, or call
   `GET $BASE/api/v1/workspaces/{ws}/sources/{id}/streams` for `freshness.state`,
   `last_success_at` and `threshold_seconds`.
3. `GET $BASE/api/v1/workspaces/{ws}/sources/{id}/runs?stream=` lists the runs with their error
   codes.

### Likely causes

1. The source is down, its credential expired or it rate-limits us (the run error code says
   which).
2. The scheduler is not running, so no run is started.
3. The threshold is too tight for the source's schedule (`threshold_is_default` is false).
4. Runs fail on a bad record (`reject_threshold`).

### Fix

1. Fix the cause named by the error code: renew the credential, or allow the host under
   `egress.allow` if the egress policy refuses it.
2. Start `zirv-flow scheduler`, then run the stream now with
   `POST $BASE/api/v1/workspaces/{ws}/sources/{id}/runs` and `{"stream": "name"}`.
3. Set a realistic `staleness_threshold_seconds` on the stream with
   `PUT .../streams/{name}`.

### Fixed when

The next run succeeds, `zirv_flow_stream_state` returns to 0 and the freshness age drops.

### Escalate

Escalate when the source owner confirms the source is healthy and runs still fail with an error
code you cannot map to a cause.

## ZirvFlowSyncFailing

### What it means

At least 3 sync runs failed in the last hour and over half of the runs in that hour failed.

### User-visible effect

Data from external sources stops arriving. This is the instance-wide view; ZirvFlowStreamStale
names the stream.

### Confirm

1. Metric: `sum by (outcome) (increase(zirv_flow_sync_runs_total[1h]))`.
2. `sum by (source_type, outcome) (increase(zirv_flow_source_requests_total[1h]))` and
   `zirv_flow_source_retries_total` show which source type and why.
3. List runs with `GET $BASE/api/v1/workspaces/{ws}/sources/{id}/runs` and read the error codes.

### Likely causes

1. A shared dependency failed: DNS, the network, the egress policy
   (`zirv_flow_egress_denied_total`).
2. A source provider outage or a revoked token (`zirv_flow_source_oauth_total`).
3. A bad release changed the mapping or the handler.

### Fix

1. Fix the shared dependency; allowed hosts are the `egress.allow` list.
2. Reconnect the source in the web app when its token was revoked.
3. Roll back the release if the failures began at a deploy, then run the failed streams again
   with `POST .../runs`.

### Fixed when

New runs succeed: `increase(zirv_flow_sync_runs_total{outcome="succeeded"}[1h])` rises and the
failed share falls under half.

### Escalate

Escalate when failures began at a deploy you cannot roll back, or span source types with no
shared cause.

## ZirvFlowSegmentDirtyBacklog

### What it means

Either the oldest dirty mark in the segment dirty set is over 15 minutes old, or more than 50000
people are marked dirty, for 15 minutes. People are marked dirty when their data changes, and
`segment.reevaluate` work clears the marks.

### User-visible effect

Segment membership lags behind the data, so segment-triggered flows start late and counts on the
segment pages are behind.

### Confirm

1. Metrics: `zirv_flow_segment_dirty_age_seconds`, `zirv_flow_segment_dirty_backlog` and
   `histogram_quantile(0.99, sum by (le) (rate(zirv_flow_segment_refresh_seconds_bucket[10m])))`.
2. `GET $BASE/api/v1/admin/jobs`: the kinds `segment.reevaluate`, `segment.refresh` and
   `segment.safety_net`.

### Likely causes

1. The segment kinds are queued or dead (see ZirvFlowJobsBacklog and ZirvFlowJobsDead).
2. A bulk import or sync dirtied many people at once; it drains by itself.
3. A very expensive segment makes each refresh slow.
4. The database is slow.

### Fix

1. Clear the queue or the dead jobs of the segment kinds, as in the queue runbooks.
2. After a bulk change, wait: the backlog should fall steadily. Add workers if it does not.
3. Simplify the segment whose refresh is slowest.

### Fixed when

`zirv_flow_segment_dirty_age_seconds` falls under 15 minutes and the backlog under 50000.

### Escalate

Escalate when the backlog is flat or growing with healthy workers and no bulk load.

## ZirvFlowDecisionLogSize

### What it means

The decision-log partitions take more than 200 GB on disk (`zirv_flow_decision_log_bytes`),
refreshed by the daily `decisions.retention` job.

### User-visible effect

None yet. A full disk stops the database, and with it everything.

### Confirm

1. Metrics: `zirv_flow_decision_log_bytes`, `zirv_flow_decision_log_partitions`, and
   `zirv_flow_retention_last_run_timestamp_seconds{table="decision_records"}`: the last run should
   be within a day.
2. Probe the size per partition:
   `psql "$DATABASE_URL" -c "SELECT c.relname, pg_total_relation_size(c.oid) FROM pg_class c JOIN pg_inherits i ON i.inhrelid = c.oid JOIN pg_class p ON p.oid = i.inhparent WHERE p.relname = 'decision_records' ORDER BY 2 DESC"`.
3. Search the scheduler logs for `could not create the decision-log partitions`.

### Likely causes

1. Traffic grew and the retention window `retention.decision_days` holds more than planned.
2. `decisions.retention` does not run or is dead (see ZirvFlowJobsDead), so old partitions stay.
3. A drop of a partition was skipped for a table lock and logged; the next daily run retries.

### Fix

1. Retry the dead `decisions.retention` job once its cause is fixed, or wait for the next
   daily run.
2. Shorten `retention.decision_days` (within `retention.decision_max_days`), restart, and let the
   next run drop the older partitions. Dropped partitions are not brought back by raising the
   value again.
3. Grow the disk if the window is needed; the README Retention windows section has the measured
   bytes per record for sizing.

### Fixed when

`zirv_flow_decision_log_bytes` falls under the threshold after the next retention run, or the
disk has room for the growth.

### Escalate

Escalate at once when free disk on the database host is under 15 percent: do not wait for the
daily job.

## ZirvFlowDestinationQueue

### What it means

A destination has items waiting for delivery and the oldest has waited over 30 minutes, for 15
minutes.

### User-visible effect

Messages or calls to that destination are late. Items stay queued; they are retried.

### Confirm

1. Metrics: `zirv_flow_destination_queue_oldest_seconds`, `zirv_flow_destination_queue_depth` and
   `sum by (destination, outcome) (increase(zirv_flow_destination_outcomes_total[30m]))`.
2. In the web app open the destination and run its check, or call
   `GET $BASE/api/v1/workspaces/{ws}/destinations/{id}/stats` and
   `POST .../destinations/{id}/check`.

### Likely causes

1. The destination is down or rate-limiting: outcomes are failures or retries.
2. The destination's credential expired.
3. The egress policy refuses the host (`zirv_flow_egress_denied_total`).
4. The send job kinds are backlogged (see ZirvFlowJobsBacklog).

### Fix

1. Renew the credential and run the check again.
2. Add the destination host to `egress.allow` when the policy blocks it, and restart.
3. Clear the job backlog. Items are delivered by their retries; nothing is re-driven by hand.

### Fixed when

The oldest age falls and `zirv_flow_destination_queue_depth` drops to its normal level.

### Escalate

Escalate when the destination provider reports an outage on their side or the queue grows with a
passing check.

## ZirvFlowForwardingLag

### What it means

The oldest forwarding outbox row for a destination has been past due for over 10 minutes, for 10
minutes.

### User-visible effect

Events that forwarding rules copy to the destination arrive late.

### Confirm

1. Metrics: `zirv_flow_destination_forwarding_lag_seconds` and
   `zirv_flow_destination_forwarding_depth`.
2. `GET $BASE/api/v1/workspaces/{ws}/forwarding/status` gives each destination's `depth`,
   `oldest_lag_seconds` and `dead` count.
3. `GET $BASE/api/v1/admin/jobs`: the `event.forward` kind.

### Likely causes

1. `event.forward` jobs are queued, or the workers are saturated (see ZirvFlowJobsBacklog).
2. The destination is failing, so rows retry with backoff (see ZirvFlowDestinationQueue).
3. The scheduler is not running, so no forwarding job is queued (the log says
   `could not queue a forwarding job` or `could not read the forwarding queues`).

### Fix

1. Start workers and `zirv-flow scheduler` as needed.
2. Fix the destination as under ZirvFlowDestinationQueue.

### Fixed when

`zirv_flow_destination_forwarding_lag_seconds` returns to 0 and the depth falls.

### Escalate

Escalate when the lag grows with a healthy destination and healthy workers.

## ZirvFlowForwardingDead

### What it means

Forwarding outbox rows were settled as dead letters in the last hour: they used up their
attempts.

### User-visible effect

Those events were not delivered to the destination. They are kept and can be driven again.

### Confirm

1. Metric: `increase(zirv_flow_destination_forwarding_dead_total[1h])`.
2. `GET $BASE/api/v1/workspaces/{ws}/forwarding/dead-letters?limit=50` lists dead rows with
   the error code, attempts and times; never the payload. `GET .../forwarding/status` gives the
   dead count per destination.

### Likely causes

1. A long destination outage outlasted the retries.
2. The destination rejects what is sent (a permanent error code).
3. A revoked credential.

### Fix

1. Fix the destination or the credential, and run its check.
2. Put the rows back in the queue with
   `POST $BASE/api/v1/workspaces/{ws}/forwarding/re-drive` and `{"destination_id": "..."}`
   (or `{"id": "..."}` for one row). It writes no event and starts no flow.
3. If the destination rejects the content permanently, leave those rows.

### Fixed when

The dead counter stops rising and `dead` in `GET .../forwarding/status` falls to 0 after the
re-drive.

### Escalate

Escalate when re-driven rows die again with the same code and the destination looks healthy.

## ZirvFlowRenderRestarts

### What it means

The render host restarted more than 3 times in 15 minutes. A child is replaced when it exceeds
`render.deadline_ms`, runs out of memory, or crashes.

### User-visible effect

Some previews, test sends and sends fail or are slow while children restart.

### Confirm

1. Metrics: `increase(zirv_flow_render_restarts_total[15m])`,
   `sum by (reason) (increase(zirv_flow_render_failures_total[15m]))` and
   `histogram_quantile(0.99, sum by (le) (rate(zirv_flow_render_wait_seconds_bucket[10m])))`.
2. Search the logs for `could not start a render host` and for log lines whose target ends in `::child`, which carry
   the render child's own output.

### Likely causes

1. A template that is too big or too slow: renders hit `render.deadline_ms`.
2. `render.memory_limit_mib` is too low for the templates (Linux only).
3. The `zirv-flow` binary cannot be started as `render-host` (a bad container image or path).
4. `render.pool_size` is too small, so waits pile up.

### Fix

1. Find the template whose render fails (`zirv_flow_render_failures_total{reason}`) and simplify
   it.
2. Raise `render.memory_limit_mib` (64 to 1048576) or `render.deadline_ms` (1 to 600000), and
   restart.
3. Raise `render.pool_size` (1 to 32) when waits are long and there are no failures.

### Fixed when

`increase(zirv_flow_render_restarts_total[15m])` is 0 or 1 and `zirv_flow_render_failures_total`
stops rising.

### Escalate

Escalate when restarts continue with default limits and simple templates: the render host
itself is crashing.

## ZirvFlowDbPoolSaturated

### What it means

Over 90 percent of a database pool's connections are in use for 10 minutes. The `main` pool is
the one HTTP and admin work use, limited by `database.max_connections`.

### User-visible effect

Requests wait for a connection, answer slowly or time out; `/readyz` can time out and answer
503. Workers use pools of their own, so a saturated main pool alone does not stop jobs.

### Confirm

1. Metrics: `zirv_flow_db_pool_connections{state="in_use"}` against
   `zirv_flow_db_pool_max_connections`, and the p99 of `zirv_flow_http_request_duration_seconds`.
2. Probe:
   `psql "$DATABASE_URL" -c "SELECT state, wait_event_type, count(*) FROM pg_stat_activity GROUP BY 1, 2 ORDER BY 3 DESC"`.
   Many `idle in transaction` or `Lock` waits point at one slow statement.

### Likely causes

1. Traffic grew beyond `database.max_connections`.
2. Slow statements hold connections (a long explorer query, a lock wait).
3. The database server is slow, or its own connection limit is reached.
4. A connection leak after a release.

### Fix

1. Raise `database.max_connections` and restart, keeping the sum over all processes, plus the
   worker pools, under the server's `max_connections`.
2. Cancel the long-running statements found by the probe.
3. Roll back a release that began the saturation.

### Fixed when

`in_use` over the maximum stays under 0.9 for 10 minutes and request latency is back to normal.

### Escalate

Escalate when the pool is full with no slow statement and no traffic growth.

## ZirvFlowNotReady

### What it means

`/readyz` answered 503 in the last 5 minutes, for 5 minutes. It is 503 when the database is
unreachable, a check takes over 2 seconds, the schema is not up to date with this binary, or (on a
`worker` or `scheduler`) a role loop has stopped or shutdown has begun. `serve`, `all` and the
`ops.bind` listener of a `worker` or `scheduler` all count their requests, so scrape every
process.

### User-visible effect

The instance is out of the load balancer or failing its health check; users see errors.

### Confirm

1. `curl -s -o /dev/null -w "%{http_code}\n" $BASE/readyz`, and without `-o` the problem body
   names the reason: `database unreachable`, `database check timed out` or
   `database schema is not up to date`.
2. `zirv-flow healthcheck` runs the same check from the host.
3. Metric: `sum by (status) (increase(zirv_flow_http_requests_total{route="/readyz"}[5m]))`.

### Likely causes

1. The database is down or the network to it is cut.
2. The database is overloaded (see ZirvFlowDbPoolSaturated).
3. A deploy started a new binary before its migrations ran.
4. An old binary runs against a database a newer one migrated.

### Fix

1. Restore the database or the network path.
2. Apply the migrations: `zirv-flow migrate`, or set `database.migrate_on_start` for the next
   start.
3. Replace old binaries with the version the schema belongs to; never edit the migrations table.

### Fixed when

`/readyz` answers 200 and the counter with `status="503"` stops rising.

### Escalate

Escalate immediately when the database is unreachable, and when `/readyz` still fails after
migrations ran.
