-- Variant B: the same 3 hot traits promoted to typed columns with plain btree indexes, the
-- other 17 traits in `traits jsonb`. Expects :schema. Run after gen.sql in the same session.
SET search_path = :"schema", pg_catalog;

CREATE TABLE people (
    id        bigint PRIMARY KEY,
    country   text,
    ltv       numeric,
    signup_at timestamptz,
    traits    jsonb NOT NULL
);
INSERT INTO people (id, country, ltv, signup_at, traits)
SELECT id,
       traits ->> 'country',
       (traits ->> 'ltv')::numeric,
       (traits ->> 'signup_at')::timestamptz,
       traits - ARRAY['country', 'ltv', 'signup_at']
FROM raw;
DROP TABLE raw;
CREATE INDEX people_country_idx ON people (country);
CREATE INDEX people_ltv_idx ON people (ltv);
CREATE INDEX people_signup_idx ON people (signup_at);
VACUUM (ANALYZE) people;
