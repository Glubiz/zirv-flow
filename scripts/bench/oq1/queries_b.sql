-- Variant B measured statements; the same names, rows and selectivities as queries_a.sql. See
-- there for the format.

-- name: eq_count
SELECT count(*) FROM people WHERE country = 'C07'

-- name: eq_ids
SELECT id FROM people WHERE country = 'C07'

-- name: range_count
SELECT count(*) FROM people WHERE ltv >= 990

-- name: range_ids
SELECT id FROM people WHERE ltv >= 990

-- name: and_count
SELECT count(*) FROM people
WHERE country = 'C07' AND ltv >= 500 AND signup_at >= timestamptz '2025-01-01T00:00:00Z'

-- name: and_ids
SELECT id FROM people
WHERE country = 'C07' AND ltv >= 500 AND signup_at >= timestamptz '2025-01-01T00:00:00Z'

-- name: upsert
-- :upserts trait changes: nine in ten patch an existing person (three hot traits and one cold
-- one), one in ten creates a person.
INSERT INTO people (id, country, ltv, signup_at, traits)
SELECT CASE WHEN g % 10 = 0 THEN :people + g ELSE 1 + (g * 7919) % :people END,
       'C' || lpad((g % 50)::text, 2, '0'),
       (g % 1000)::numeric,
       timestamptz '2026-01-01T00:00:00Z',
       jsonb_build_object('orders', g % 100)
FROM generate_series(1, :upserts) g
ON CONFLICT (id) DO UPDATE SET
    country = EXCLUDED.country,
    ltv = EXCLUDED.ltv,
    signup_at = EXCLUDED.signup_at,
    traits = people.traits || EXCLUDED.traits
