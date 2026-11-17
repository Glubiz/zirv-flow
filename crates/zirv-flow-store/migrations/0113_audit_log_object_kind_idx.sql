-- The audit list filters by `object_kind` (the object-kind filter of the audit page and the
-- export), and every index of `audit_log` that leads with `workspace_id` is on another column
-- (`id`, `occurred_at`, `action`, `actor_id`, `object_id`), so the filter had no index to seek:
-- the planner read the workspace's whole log. Measured on PostgreSQL 16 with 2,000,000 rows in one
-- workspace and 40 rows of a rare kind: 118 ms and 42,000 buffers (a sequential scan, cost
-- linear in the table). This index serves the filter in id order, like its siblings.
--
-- Write cost: `audit_log` is written by every audited action, and this is its eighth index.
-- Measured insert loop of the audited shape (single-row inserts, 20 or 200 to a transaction, three
-- runs each, medians): 7.09 ms against 7.25 ms a 20-insert transaction and 61.7 ms against 61.2 ms
-- a 200-insert one, inside the run-to-run spread. The numbers are in `docs/operations/upgrade.md`.
--
-- UPGRADE COST: the build takes SHARE on `audit_log` until the migration commits, which blocks
-- every append (and so every audited operation) for the build; it scans the table once (1.7 s
-- for 2,000,000 rows). A populated deployment builds the index first, by hand and outside a
-- transaction, with the statement below; the migration then finds it and does nothing:
--
--     CREATE INDEX CONCURRENTLY IF NOT EXISTS audit_log_workspace_kind_idx
--         ON audit_log (workspace_id, object_kind, id);
--
-- The migration keeps an index of that name only when it is valid and is this index; one a failed
-- or cancelled build left invalid, or a different index under the name, is dropped and built
-- again here.
DO $$
BEGIN
    IF to_regclass('audit_log_workspace_kind_idx') IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM pg_index
        WHERE indexrelid = to_regclass('audit_log_workspace_kind_idx')
          AND indisvalid AND NOT indisunique AND indrelid = 'audit_log'::regclass
          AND split_part(pg_get_indexdef(indexrelid), ' USING ', 2)
              = 'btree (workspace_id, object_kind, id)'
    ) THEN
        DROP INDEX audit_log_workspace_kind_idx;
    END IF;
END
$$;
CREATE INDEX IF NOT EXISTS audit_log_workspace_kind_idx ON audit_log (workspace_id, object_kind, id);
