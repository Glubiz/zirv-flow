-- Seed for the ignored MySQL source tests. __DB__ and __USER__ are replaced with a random name per
-- test; the test drops both when it ends. Statements end with a semicolon at the end of a line and
-- the test sends them one by one.
SET SESSION cte_max_recursion_depth = 5000;
CREATE DATABASE __DB__;
CREATE DATABASE __DB___other;

-- 1,200 rows. Ids 493 to 508 share one updated_at, which straddles the first batch boundary
-- (500). Id 1200 has a NULL updated_at. Id 7 holds a sentinel no error text may contain.
CREATE TABLE __DB__.contacts (
    id BIGINT NOT NULL PRIMARY KEY,
    email VARCHAR(255) NOT NULL,
    updated_at DATETIME(6) NULL,
    note TEXT NULL,
    payload BLOB NULL
);
INSERT INTO __DB__.contacts
WITH RECURSIVE seq (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 1200)
SELECT n,
       CONCAT('user', n, '@example.com'),
       CASE WHEN n = 1200 THEN NULL
            WHEN n BETWEEN 493 AND 508 THEN TIMESTAMP('2024-01-01 00:00:00') + INTERVAL 493 MINUTE
            ELSE TIMESTAMP('2024-01-01 00:00:00') + INTERVAL n MINUTE END,
       CASE WHEN n = 7 THEN 'sentinel-secret-value' ELSE CONCAT('note ', n) END,
       CASE WHEN n = 1 THEN X'DEADBEEF' ELSE NULL END
FROM seq;

CREATE VIEW __DB__.contacts_view AS SELECT id, email FROM __DB__.contacts;
CREATE TABLE __DB___other.hidden (id BIGINT NOT NULL PRIMARY KEY);
CREATE TABLE __DB__.no_key (a INT, b VARCHAR(20));
INSERT INTO __DB__.no_key VALUES (1, 'x');

-- A composite primary key.
CREATE TABLE __DB__.pairs (
    region VARCHAR(10) NOT NULL,
    code INT NOT NULL,
    label VARCHAR(20),
    updated_at DATETIME,
    PRIMARY KEY (region, code)
);
INSERT INTO __DB__.pairs
SELECT r.region, c.n, CONCAT(r.region, c.n), TIMESTAMP('2024-02-01 00:00:00') + INTERVAL c.n HOUR
FROM (SELECT 'eu' AS region UNION ALL SELECT 'us') AS r,
     (SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6) AS c;
-- A row with no cursor value, which sorts inside the one page of this small table.
INSERT INTO __DB__.pairs VALUES ('zz', 1, 'no cursor', NULL);

-- Integer and decimal keys, signed and unsigned.
CREATE TABLE __DB__.numbers (
    big BIGINT UNSIGNED NOT NULL,
    price DECIMAL(20, 4) NOT NULL,
    PRIMARY KEY (big)
);
INSERT INTO __DB__.numbers VALUES (18446744073709551615, 1.5000), (9007199254740993, 2.2500), (9007199254740992, 3.0000);

-- The user the source connects as: it can only SELECT.
CREATE USER '__USER__'@'%' IDENTIFIED BY 'zf_test_pw';
GRANT SELECT ON __DB__.* TO '__USER__'@'%';
