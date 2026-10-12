-- Variant A: every trait in `traits jsonb`, expression indexes on the 3 hot traits
-- (country: text equality, ltv: numeric range, signup_at: timestamp range).
-- Expects :schema. Run after gen.sql in the same session.
SET search_path = :"schema", pg_catalog;

-- An index on a text-to-timestamptz cast is refused, because the cast depends on settings
-- (DateStyle, TimeZone) and is only STABLE. The wrapper promises what is true of the stored
-- values: ISO 8601 with an explicit UTC offset. Queries must call it for the index to apply.
CREATE FUNCTION oq1_ts(text) RETURNS timestamptz
    LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT $1::timestamptz $$;

ALTER TABLE raw RENAME TO people;
ALTER TABLE people ADD PRIMARY KEY (id);
ALTER TABLE people ALTER COLUMN traits SET NOT NULL;
CREATE INDEX people_country_idx ON people ((traits ->> 'country'));
CREATE INDEX people_ltv_idx ON people (((traits ->> 'ltv')::numeric));
CREATE INDEX people_signup_idx ON people (oq1_ts(traits ->> 'signup_at'));
VACUUM (ANALYZE) people;
