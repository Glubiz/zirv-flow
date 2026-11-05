-- What the incremental phase's load did to the workspace, read after the drain (psql -v ws=... -v since=...).
-- Separates "the events never reached a seeded person" (people_created_since large) from "they reached people
-- but no entry followed" (events_since large, people_created_since near zero). One JSON object; nothing written.
SELECT json_build_object(
    'orders_since', (SELECT count(*) FROM events
        WHERE workspace_id = :'ws' AND name = 'bench_order_placed'
          AND occurred_at >= :'since'::timestamptz - interval '1 minute'),
    'people_created_since', (SELECT count(*) FROM people
        WHERE workspace_id = :'ws' AND created_at >= :'since'::timestamptz - interval '1 minute'));
