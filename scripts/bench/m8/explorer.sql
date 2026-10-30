-- Fixed synthetic M8 queries, transcribed from events/explore/mod.rs: push_scope,
-- explore_count, explore_breakdown (both statements), explore_rows and explore_total.
-- psql quotes parameters as literals. No product data is used as SQL text.
\set ON_ERROR_STOP on
SET timezone = 'UTC';
BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;
-- Plans are collected outside the timed ingest window; allow slow plans to finish.
SET LOCAL statement_timeout = '15min';
SELECT :'shape' IN ('count_name', 'count_property', 'count_person') AS is_count,
       :'shape' IN ('breakdown_low', 'breakdown_high') AS is_breakdown,
       :'shape' = 'export' AS is_export,
       :'shape' = 'count_property' AS by_property,
       :'shape' = 'count_person' AS by_person,
       CASE WHEN :'shape' = 'breakdown_low' THEN 'category' ELSE 'amount' END AS property
\gset
-- Resolve the person group as people/timeline.rs does, then bind ANY to that UUID array.
WITH RECURSIVE group_ AS (
  SELECT id FROM people WHERE workspace_id = :'ws'::uuid AND id = (
    SELECT COALESCE(merged_into, id) FROM people WHERE workspace_id = :'ws'::uuid AND id = :'person'::uuid)
  UNION
  SELECT p.id FROM people p JOIN group_ g ON p.merged_into = g.id WHERE p.workspace_id = :'ws'::uuid
)
SELECT array_agg(id)::text AS people FROM group_
\gset
SELECT format(' FROM events e WHERE e.workspace_id = %L::uuid AND e.occurred_at >= %L::timestamptz AND e.occurred_at < %L::timestamptz AND e.name = %L',
              :'ws', :'from', :'to', 'bench_order_placed') AS scope
\gset
\if :by_property
SELECT :'scope' || ' AND (e.properties ->> ''category''::text) = ''books''::text' AS scope \gset
\endif
\if :by_person
SELECT :'scope' || format(' AND e.person_id = ANY(%L::uuid[])', :'people') AS scope \gset
\endif
\if :is_count
EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)
SELECT date_trunc('day', e.occurred_at AT TIME ZONE 'UTC') AT TIME ZONE 'UTC' AS bucket, count(*) AS n
:scope GROUP BY 1 ORDER BY 1 LIMIT 10001;
\elif :is_breakdown
EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)
SELECT (e.properties ->> :'property'::text) AS v, count(*) AS n
:scope AND COALESCE(jsonb_typeof((e.properties -> :'property'::text)), 'null') <> 'null'
GROUP BY 1 ORDER BY n DESC, v LIMIT 50;
EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)
SELECT count(*) AS total,
       count(*) FILTER (WHERE COALESCE(jsonb_typeof((e.properties -> :'property'::text)), 'null') <> 'null') AS present
:scope;
\else
\if :is_export
EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)
SELECT count(*) :scope;
\set page_size 1001
\else
\set page_size 101
\endif
EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)
SELECT e.id, e.occurred_at, e.name, e.source, e.person_id, e.validation_state,
       e.flags, e.trace_id, e.properties, e.schema_version
:scope ORDER BY e.occurred_at DESC, e.id DESC LIMIT :page_size;
\if :is_export
-- Also show the keyset query of a later export page, using the first page's last row.
SELECT max(at) AS at, max(id::text) AS id FROM (
  SELECT e.occurred_at AS at, e.id
  :scope ORDER BY e.occurred_at DESC, e.id DESC LIMIT 1 OFFSET 999
) last_row
\gset cursor_
\if :{?cursor_id}
EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)
SELECT e.id, e.occurred_at, e.name, e.source, e.person_id, e.validation_state,
       e.flags, e.trace_id, e.properties, e.schema_version
:scope AND (e.occurred_at, e.id) < (:'cursor_at'::timestamptz, :'cursor_id'::uuid)
ORDER BY e.occurred_at DESC, e.id DESC LIMIT 1001;
\endif
\endif
\endif
ROLLBACK;
