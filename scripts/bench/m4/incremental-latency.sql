-- Event-to-entry latency of the incremental re-evaluation (spec #127 D4.7; architecture 8.4,
-- section 19 "event to segment.entered"), from the database after a loadgen run. Nothing is
-- written. In the style of ../applied-latency.sql:
--
--   psql -v ws=<workspace> -v segment=<segment id> -v since=<ISO instant> -f incremental-latency.sql
--
-- For every `segment.entered` decision of the segment that an event caused (cause_kind `event`;
-- the entries of the first refresh have another cause and are not counted), the latency is
-- decision_records.recorded_at minus the causing event's received_at. cause_id is the id of the
-- latest event of the person that the entry coalesced. recorded_at defaults to now(), the START
-- of the re-evaluating transaction, so the figure leaves out the time a person waits inside a
-- batch of that job: a slight under-estimate, as in applied-latency.sql.
-- `since` bounds both sides; the events are read by occurred_at (loadgen leaves it to the
-- server, so it equals the arrival) so that only the recent partitions are read.
-- One JSON object on standard output.
SELECT json_build_object(
    'entries_caused_by_events', count(m.seconds),
    'p50_s', round((percentile_cont(0.50) WITHIN GROUP (ORDER BY m.seconds))::numeric, 3),
    'p95_s', round((percentile_cont(0.95) WITHIN GROUP (ORDER BY m.seconds))::numeric, 3),
    'p99_s', round((percentile_cont(0.99) WITHIN GROUP (ORDER BY m.seconds))::numeric, 3),
    'max_s', round(max(m.seconds)::numeric, 3),
    'entries_total', (SELECT count(*) FROM decision_records
        WHERE workspace_id = :'ws' AND action = 'segment.entered'
          AND subject_ids ->> 'segment' = :'segment' AND recorded_at >= :'since'),
    'exits_total', (SELECT count(*) FROM decision_records
        WHERE workspace_id = :'ws' AND action = 'segment.exited'
          AND subject_ids ->> 'segment' = :'segment' AND recorded_at >= :'since'))
FROM (
    SELECT extract(epoch FROM d.recorded_at - e.received_at) AS seconds
    FROM decision_records d
    JOIN events e ON e.workspace_id = d.workspace_id AND e.id::text = d.cause_id
                 AND e.occurred_at >= :'since'::timestamptz - interval '1 minute'
    WHERE d.workspace_id = :'ws' AND d.action = 'segment.entered' AND d.cause_kind = 'event'
      AND d.subject_ids ->> 'segment' = :'segment' AND d.recorded_at >= :'since'
) m;
