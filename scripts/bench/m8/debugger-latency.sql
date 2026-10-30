-- Durable inbox append to debugger receipt (architecture sections 17 and 19, issue #394).
-- Run after the watcher finishes and the inbox drains, on the SAME HOST as loadgen:
--   psql -X "$DATABASE_URL" -v receive_file=/absolute/path/receive.jsonl -f scripts/bench/m8/debugger-latency.sql
-- Requires python3 on that host for a contemporaneous clock comparison, not a comparison
-- of an old receipt with today's now(). Do not use psql --single-transaction: now() must
-- be sampled in a fresh transaction. Stable clocks from the run through this check are
-- assumed; start/end wall time versus monotonic elapsed also detects a run-time clock step.
-- JSONL timestamps are UTC Unix microseconds. Null event ids cannot join to stored events.
-- Every receipt is retained in the file; reconnect replays count only once per client/event.
-- Delivered share = distinct delivered client/event pairs / (watchers * stored API events
-- whose inbox arrival is inside the recorded run). Start watchers before offering load and
-- leave time to drain before they stop. Rejected events have no events row and are excluded.
-- percentile_disc is nearest rank, matching loadgen's report::Latency, without interpolation.
\set ON_ERROR_STOP on
\if :{?receive_file}
\else
    \echo 'required: -v receive_file=/absolute/path/receive.jsonl'
    DO $$ BEGIN RAISE EXCEPTION 'receive_file is required'; END $$;
\endif

CREATE TEMP TABLE debugger_receipts (record jsonb NOT NULL);
-- \copy does not interpolate psql variables; pass the path through the environment so
-- spaces and shell metacharacters stay data, including in a path supplied with -v.
\setenv ZIRV_DEBUGGER_RECEIVE_FILE :receive_file
\copy debugger_receipts FROM PROGRAM 'cat -- "$ZIRV_DEBUGGER_RECEIVE_FILE"' WITH (FORMAT csv, DELIMITER E'\x02', QUOTE E'\x01')

SELECT count(*) FILTER (WHERE record->>'kind' = 'started') = 1
   AND count(*) FILTER (WHERE record->>'kind' = 'finished') = 1 AS complete
FROM debugger_receipts
\gset
\if :complete
\else
    DO $$ BEGIN RAISE EXCEPTION 'receive file must contain one completed run'; END $$;
\endif

CREATE TEMP TABLE debugger_run AS
SELECT (s.record->>'workspace')::uuid AS workspace_id,
       (s.record->>'clients')::integer AS clients,
       (s.record->>'received_at')::numeric AS start_us,
       (f.record->>'received_at')::numeric AS end_us,
       (f.record->>'elapsed_us')::numeric AS elapsed_us
FROM debugger_receipts s CROSS JOIN debugger_receipts f
WHERE s.record->>'kind' = 'started' AND f.record->>'kind' = 'finished';

SELECT COALESCE(clients > 0 AND end_us > start_us AND elapsed_us > 0
       AND (SELECT count(*) FROM debugger_receipts WHERE record->>'kind' = 'client') = clients
       AND (SELECT count(DISTINCT (record->>'client')::integer)
            FROM debugger_receipts
            WHERE record->>'kind' = 'client'
              AND (record->>'client')::integer BETWEEN 1 AND clients
              AND (record->'counts'->>'connections')::bigint > 0) = clients, false) AS valid_run
FROM debugger_run
\gset
\if :valid_run
\else
    DO $$ BEGIN RAISE EXCEPTION 'invalid run boundaries or missing connected clients'; END $$;
\endif

\set client_host `python3 -I -c 'import socket; print(socket.gethostname().split(".")[0].lower())'`
SELECT COALESCE(bool_and(record->>'host' = :'client_host'), false) AS same_host
FROM debugger_receipts WHERE record->>'kind' = 'started'
\gset
\if :same_host
\else
    DO $$ BEGIN RAISE EXCEPTION 'psql host differs from the watcher host recorded in the receipt'; END $$;
\endif

\set client_before_us `python3 -I -c 'import time; print(time.time_ns() // 1000)'`
SELECT extract(epoch FROM now()) * 1000000 AS db_now_us
\gset
\set client_after_us `python3 -I -c 'import time; print(time.time_ns() // 1000)'`

CREATE TEMP TABLE debugger_clock AS
SELECT (:'client_before_us'::numeric + :'client_after_us'::numeric) / 2 - :db_now_us AS offset_us,
       (:'client_after_us'::numeric - :'client_before_us'::numeric) / 2 AS uncertainty_us;

SELECT to_timestamp(start_us / 1000000) AS client_started_at,
       to_timestamp(end_us / 1000000) AS client_finished_at,
       to_timestamp(:db_now_us / 1000000) AS database_now,
       round(offset_us / 1000, 3) AS client_minus_database_ms,
       round(uncertainty_us / 1000, 3) AS clock_uncertainty_ms,
       round((end_us - start_us - elapsed_us) / 1000, 3) AS run_clock_step_ms
FROM debugger_run CROSS JOIN debugger_clock;

SELECT COALESCE(uncertainty_us >= 0 AND abs(offset_us) + uncertainty_us <= 50000
       AND abs(end_us - start_us - elapsed_us) <= 50000, false) AS clock_ok
FROM debugger_run CROSS JOIN debugger_clock
\gset
\if :clock_ok
\else
    DO $$ BEGIN RAISE EXCEPTION 'clock skew, sampling uncertainty or run clock step exceeds 50 ms'; END $$;
\endif

CREATE TEMP TABLE debugger_events AS
SELECT e.id, e.received_at
FROM events e CROSS JOIN debugger_run r
WHERE e.workspace_id = r.workspace_id AND e.source = 'api'
  AND e.received_at >= to_timestamp(r.start_us / 1000000)
  AND e.received_at < to_timestamp(r.end_us / 1000000);

WITH first_receipt AS (
    SELECT (record->>'client')::integer AS client,
           (record->>'event_id')::uuid AS event_id,
           to_timestamp(min((record->>'received_at')::numeric) / 1000000) AS received_at_client
    FROM debugger_receipts
    WHERE record->>'kind' = 'item' AND record->>'event_id' IS NOT NULL
    GROUP BY (record->>'client')::integer, (record->>'event_id')::uuid
), measured AS (
    SELECT f.client, extract(epoch FROM f.received_at_client - e.received_at) AS seconds
    FROM first_receipt f JOIN debugger_events e ON e.id = f.event_id
), labels AS (
    SELECT generate_series(1, clients) AS client FROM debugger_run
    UNION ALL
    SELECT NULL::integer
), expected AS (
    SELECT count(*) AS events FROM debugger_events
)
SELECT COALESCE(l.client::text, 'total') AS client,
       count(m.seconds) AS items,
       round(percentile_disc(0.50) WITHIN GROUP (ORDER BY m.seconds) * 1000, 3) AS p50_ms,
       round(percentile_disc(0.95) WITHIN GROUP (ORDER BY m.seconds) * 1000, 3) AS p95_ms,
       round(percentile_disc(0.99) WITHIN GROUP (ORDER BY m.seconds) * 1000, 3) AS p99_ms,
       round(max(m.seconds) * 1000, 3) AS max_ms,
       e.events * CASE WHEN l.client IS NULL THEN r.clients ELSE 1 END AS expected_deliveries,
       round(count(m.seconds)::numeric /
             NULLIF(e.events * CASE WHEN l.client IS NULL THEN r.clients ELSE 1 END, 0), 6) AS delivered_share
FROM labels l CROSS JOIN expected e CROSS JOIN debugger_run r
LEFT JOIN measured m ON l.client IS NULL OR m.client = l.client
GROUP BY l.client, e.events, r.clients
ORDER BY l.client NULLS LAST;
