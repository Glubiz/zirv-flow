-- Variant A measured statements. `-- name:` starts a statement (up to the next marker, without
-- its trailing semicolon). The runner wraps each in EXPLAIN (ANALYZE, BUFFERS) inside a
-- transaction that is rolled back, so the upsert never changes the data. Variables: :people and
-- :upserts. About 2% of rows match the equality, 1% the range and 0.3% all three.

-- name: eq_count
SELECT count(*) FROM people WHERE traits ->> 'country' = 'C07'

-- name: eq_ids
SELECT id FROM people WHERE traits ->> 'country' = 'C07'

-- name: range_count
SELECT count(*) FROM people WHERE (traits ->> 'ltv')::numeric >= 990

-- name: range_ids
SELECT id FROM people WHERE (traits ->> 'ltv')::numeric >= 990

-- name: and_count
SELECT count(*) FROM people
WHERE traits ->> 'country' = 'C07'
  AND (traits ->> 'ltv')::numeric >= 500
  AND oq1_ts(traits ->> 'signup_at') >= timestamptz '2025-01-01T00:00:00Z'

-- name: and_ids
SELECT id FROM people
WHERE traits ->> 'country' = 'C07'
  AND (traits ->> 'ltv')::numeric >= 500
  AND oq1_ts(traits ->> 'signup_at') >= timestamptz '2025-01-01T00:00:00Z'

-- name: upsert
-- :upserts trait changes: nine in ten patch an existing person (three hot traits and one cold
-- one), one in ten creates a person.
INSERT INTO people (id, traits)
SELECT CASE WHEN g % 10 = 0 THEN :people + g ELSE 1 + (g * 7919) % :people END,
       jsonb_build_object(
           'country', 'C' || lpad((g % 50)::text, 2, '0'),
           'ltv', (g % 1000)::numeric,
           'signup_at', '2026-01-01T00:00:00Z',
           'orders', g % 100)
FROM generate_series(1, :upserts) g
ON CONFLICT (id) DO UPDATE SET traits = people.traits || EXCLUDED.traits
