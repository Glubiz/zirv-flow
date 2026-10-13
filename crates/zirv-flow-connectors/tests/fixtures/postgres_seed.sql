-- Seed for the ignored PostgreSQL source tests. __SCHEMA__ and __ROLE__ are replaced with a
-- random name per test; the test drops both when it ends.
CREATE SCHEMA __SCHEMA__;
CREATE SCHEMA __SCHEMA___other;

-- 1,200 rows. Ids 493 to 508 share one updated_at, which straddles the first batch boundary
-- (500). Id 1200 has a NULL updated_at. Id 7 holds a sentinel no error text may contain.
CREATE TABLE __SCHEMA__.contacts (
    id bigint PRIMARY KEY,
    email text NOT NULL,
    updated_at timestamptz,
    note text,
    payload bytea
);
INSERT INTO __SCHEMA__.contacts
SELECT n,
       'user' || n || '@example.com',
       CASE WHEN n = 1200 THEN NULL
            WHEN n BETWEEN 493 AND 508 THEN timestamptz '2024-01-01 00:00:00+00' + interval '493 minutes'
            ELSE timestamptz '2024-01-01 00:00:00+00' + n * interval '1 minute' END,
       CASE WHEN n = 7 THEN 'sentinel-secret-value' ELSE 'note ' || n END,
       CASE WHEN n = 1 THEN '\xdeadbeef'::bytea ELSE NULL END
FROM generate_series(1, 1200) AS n;

CREATE VIEW __SCHEMA__.contacts_view AS SELECT id, email FROM __SCHEMA__.contacts;
CREATE TABLE __SCHEMA___other.hidden (id bigint PRIMARY KEY);
CREATE TABLE __SCHEMA__.no_key (a integer, b text);
INSERT INTO __SCHEMA__.no_key VALUES (1, 'x');

-- A composite primary key.
CREATE TABLE __SCHEMA__.pairs (
    region text NOT NULL,
    code integer NOT NULL,
    label text,
    updated_at timestamp,
    PRIMARY KEY (region, code)
);
INSERT INTO __SCHEMA__.pairs
SELECT r, c, r || c, timestamp '2024-02-01 00:00:00' + c * interval '1 hour'
FROM unnest(ARRAY['eu', 'us']) AS r, generate_series(1, 6) AS c;
-- A row with no cursor value, which sorts inside the one page of this small table.
INSERT INTO __SCHEMA__.pairs VALUES ('zz', 1, 'no cursor', NULL);

-- The role the source connects as: it can only SELECT.
CREATE ROLE __ROLE__ LOGIN PASSWORD 'zf_test_pw';
GRANT USAGE ON SCHEMA __SCHEMA__ TO __ROLE__;
GRANT SELECT ON ALL TABLES IN SCHEMA __SCHEMA__ TO __ROLE__;
