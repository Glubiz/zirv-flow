-- The stats of a destination (`destination_attempt_stats`) count its attempts of the last up to 168
-- hours by `outcome` and by `status_class`. Neither column was in `delivery_attempts_destination_idx
-- (workspace_id, destination_id, attempted_at)`, so every matching row was fetched from the heap:
-- 1,000,000 attempts of one destination read 29,564 buffers (a bitmap heap scan) or the whole
-- table (24,559 buffers, a parallel sequential scan). The index is rebuilt with the same key and
-- `INCLUDE (outcome, status_class)`, so the query is an index-only scan that never visits the heap.
-- The cost is cut, not removed: the scan is still linear in the window's rows, in index entries.
-- The attempts list (`ORDER BY attempted_at DESC, id DESC`) and the retention purge keep their
-- indexes: the list uses the same leading columns, the purge `delivery_attempts_attempted_at_idx`.
--
-- The old index keeps its name, so the swap is one drop and one rename: the new index is built
-- under a temporary name and takes the old name when the old one is dropped. The old index is
-- dropped in this file, so the table has no more indexes than before; its entries are wider by two
-- short text columns. The before and after numbers of the insert path and the index sizes are in
-- `docs/operations/upgrade.md`.
--
-- UPGRADE COST: the build takes SHARE on `delivery_attempts` until the migration commits, which
-- blocks every attempt insert (every send waits to record its attempt) for the build, which scans
-- the table once; reads go on. `CREATE INDEX CONCURRENTLY` cannot run in a migration, because sqlx
-- runs each file in a transaction. To keep the stop short on a large installation, build the new
-- index before it, under the temporary name (the statement below skips an index that exists):
--     CREATE INDEX CONCURRENTLY IF NOT EXISTS delivery_attempts_destination_stats_idx
--         ON delivery_attempts (workspace_id, destination_id, attempted_at)
--         INCLUDE (outcome, status_class);
-- (drop an invalid index a failed concurrent build leaves, then run it again). The migration then
-- only drops the old index and renames the new one, which takes ACCESS EXCLUSIVE on the table for
-- an instant.
CREATE INDEX IF NOT EXISTS delivery_attempts_destination_stats_idx
    ON delivery_attempts (workspace_id, destination_id, attempted_at)
    INCLUDE (outcome, status_class);
DROP INDEX delivery_attempts_destination_idx;
ALTER INDEX delivery_attempts_destination_stats_idx RENAME TO delivery_attempts_destination_idx;
