-- Inbox-to-applied latency of API ingestion, measured from the database after a run
-- (docs: spec #47 D2; architecture section 19 "Inbox to applied profile"). Nothing is added
-- to the schema and nothing is written. One statement, so any client can run it:
--
--   psql "$DATABASE_URL" -f scripts/bench/applied-latency.sql
--
-- Restrict it to one run with a lower bound on the arrival time (leave it unset to measure
-- every API item in the database):
--
--   PGOPTIONS="-c bench.since=2026-01-01T10:00:00Z" psql "$DATABASE_URL" -f scripts/bench/applied-latency.sql
--
-- track:    the apply time of the event minus events.received_at. The apply time is the
--           millisecond timestamp in the first 48 bits of the event's v7 id, which the drain
--           takes when it applies the event (no decision is written for an accepted event, and
--           the timestamp is read in SQL so this works before PostgreSQL 17); received_at is
--           the inbox row's arrival time, copied when the event is stored.
-- identify: decision_records.recorded_at of the LATEST `trait.change` decision of a
--           source record (subject_ids->>'source_record') minus the observation time the
--           record keeps for its traits ("at" in source_records.traits). The inbox row is
--           deleted when drained, and an identify without a `timestamp` is observed at its
--           arrival, so the loadgen sends none. Only the latest change per record is
--           comparable, because "at" is overwritten by every later change.
--
-- For identify, recorded_at defaults to now(), which is the START of the draining transaction, so
-- that figure leaves out the time a row waits inside its own drain batch (bounded by
-- ingest.drain_batch_size rows); it is a slight under-estimate. The track figure is taken when the
-- event is applied, so it includes part of that wait, to the millisecond.
--
-- Output, one row per kind (zero items give NULL percentiles): items, p50_ms, p95_ms,
-- p99_ms, max_ms.
WITH bounds AS (
    SELECT COALESCE(NULLIF(current_setting('bench.since', true), '')::timestamptz,
                    '-infinity'::timestamptz) AS since
),
track AS (
    SELECT 'track'::text AS kind,
           extract(epoch FROM
               to_timestamp(('x' || substr(replace(e.id::text, '-', ''), 1, 12))::bit(48)::bigint / 1000.0)
               - e.received_at) AS seconds
    FROM events e
    CROSS JOIN bounds
    WHERE e.source = 'api' AND e.received_at >= bounds.since
),
latest_change AS (
    SELECT DISTINCT ON (d.subject_ids ->> 'source_record')
           (d.subject_ids ->> 'source_record')::uuid AS source_record_id,
           d.recorded_at
    FROM decision_records d
    WHERE d.action = 'trait.change' AND d.subject_ids ? 'source_record'
    ORDER BY d.subject_ids ->> 'source_record', d.recorded_at DESC
),
identify AS (
    SELECT 'identify'::text AS kind,
           extract(epoch FROM c.recorded_at - observed.at) AS seconds
    FROM latest_change c
    JOIN source_records s ON s.id = c.source_record_id
    CROSS JOIN LATERAL (
        SELECT max((t.value ->> 'at')::timestamptz) AS at FROM jsonb_each(s.traits) t
    ) observed
    CROSS JOIN bounds
    WHERE s.source = 'api' AND s.stream = 'identify' AND observed.at >= bounds.since
),
measured AS (
    SELECT * FROM track
    UNION ALL
    SELECT * FROM identify
)
SELECT kinds.kind,
       count(m.seconds) AS items,
       round((percentile_cont(0.50) WITHIN GROUP (ORDER BY m.seconds) * 1000)::numeric, 1) AS p50_ms,
       round((percentile_cont(0.95) WITHIN GROUP (ORDER BY m.seconds) * 1000)::numeric, 1) AS p95_ms,
       round((percentile_cont(0.99) WITHIN GROUP (ORDER BY m.seconds) * 1000)::numeric, 1) AS p99_ms,
       round((max(m.seconds) * 1000)::numeric, 1) AS max_ms
FROM (VALUES ('identify'), ('track')) AS kinds (kind)
LEFT JOIN measured m ON m.kind = kinds.kind
GROUP BY kinds.kind
ORDER BY kinds.kind;
