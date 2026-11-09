-- The three queue tables turn over their whole contents in minutes: a job is inserted, claimed,
-- updated and deleted; an inbox row is inserted and deleted by the drain; a person change is
-- inserted and then consumed. At the sim's ~570 items/s their dead tuples outrun the default
-- autovacuum: it waits for 20% of the table plus 50 rows, which on a table that is mostly empty
-- never arrives soon, and when it runs it sleeps on the cost delay (samples caught
-- `autovacuum: VACUUM ANALYZE public.ingest_inbox` in `Timeout:VacuumDelay`). Meanwhile every
-- claim and inbox read walks the dead tuples: ~2,000 buffer hits for a claim and ~6,700 for the
-- inbox head read, for a handful of live rows.
--
-- So each table gets a fixed trigger instead of a fraction of its size, and no throttling:
--  * `autovacuum_vacuum_scale_factor = 0` with a threshold of dead tuples. The tables stay small,
--    so a fixed count is what bounds the dead rows a scan can meet. 2,000 for `jobs` (every job
--    makes two or three dead versions and its reads are the hottest), 5,000 for `ingest_inbox`
--    and `person_changes`, which are larger rows deleted in bulk and cost more per vacuum.
--  * `autovacuum_vacuum_cost_delay = 0`: a vacuum of a small table is short, and sleeping in it
--    is what let the dead tuples pile up. The other tables keep the default.
--  * `autovacuum_vacuum_insert_threshold = 5000`: inserted rows alone also trigger a vacuum, so
--    the visibility map is set and index-only scans stay cheap on a table that is mostly inserts.
--  * `autovacuum_analyze_scale_factor = 0.01` with a threshold of 1,000: statistics follow a
--    queue whose size swings from empty to thousands of rows, so a plan is not made on a stale
--    row count.
-- The fillfactor is left alone.
--
-- `ALTER TABLE ... SET (storage parameters)` takes SHARE UPDATE EXCLUSIVE, which does not block
-- reads or writes. All three tables are plain, not partitioned.
ALTER TABLE jobs SET (
    autovacuum_vacuum_scale_factor = 0,
    autovacuum_vacuum_threshold = 2000,
    autovacuum_vacuum_cost_delay = 0,
    autovacuum_vacuum_insert_threshold = 5000,
    autovacuum_analyze_scale_factor = 0.01,
    autovacuum_analyze_threshold = 1000
);
ALTER TABLE ingest_inbox SET (
    autovacuum_vacuum_scale_factor = 0,
    autovacuum_vacuum_threshold = 5000,
    autovacuum_vacuum_cost_delay = 0,
    autovacuum_vacuum_insert_threshold = 5000,
    autovacuum_analyze_scale_factor = 0.01,
    autovacuum_analyze_threshold = 1000
);
ALTER TABLE person_changes SET (
    autovacuum_vacuum_scale_factor = 0,
    autovacuum_vacuum_threshold = 5000,
    autovacuum_vacuum_cost_delay = 0,
    autovacuum_vacuum_insert_threshold = 5000,
    autovacuum_analyze_scale_factor = 0.01,
    autovacuum_analyze_threshold = 1000
);
