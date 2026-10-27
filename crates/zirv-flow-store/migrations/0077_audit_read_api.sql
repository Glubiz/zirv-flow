-- The audit log's read path and its append-only guard (architecture 16.2).
--
-- `trace_id` lets an entry open its trace. It is null on old rows and on rows written outside a
-- request (the command line, the jobs).
--
-- The indexes serve the filtered, keyset-paged read (id descending within a workspace) and the
-- purge (`occurred_at`). Each is built with IF NOT EXISTS so a deployment that built it first is
-- accepted.
--
-- LOCKS. Adding a nullable column without a default is a catalogue change that takes a brief
-- ACCESS EXCLUSIVE lock. Each plain CREATE INDEX takes a SHARE lock on `audit_log` until it is
-- built, which blocks every append (and so every audited operation) for the build time; that
-- grows with the table. A populated deployment builds the indexes first, by hand and outside a
-- transaction, with the statements below; the migration then finds them and does nothing:
--
--     CREATE INDEX CONCURRENTLY IF NOT EXISTS audit_log_workspace_time_idx
--         ON audit_log (workspace_id, occurred_at, id);
--     CREATE INDEX CONCURRENTLY IF NOT EXISTS audit_log_workspace_action_idx
--         ON audit_log (workspace_id, action, id);
--     CREATE INDEX CONCURRENTLY IF NOT EXISTS audit_log_workspace_actor_idx
--         ON audit_log (workspace_id, actor_id, id);
--     CREATE INDEX CONCURRENTLY IF NOT EXISTS audit_log_workspace_object_idx
--         ON audit_log (workspace_id, object_id, id);
--     CREATE INDEX CONCURRENTLY IF NOT EXISTS audit_log_request_idx
--         ON audit_log (request_id) WHERE request_id IS NOT NULL;
--     CREATE INDEX CONCURRENTLY IF NOT EXISTS audit_log_occurred_at_idx
--         ON audit_log (occurred_at);
--
-- A failed concurrent build leaves an INVALID index; drop it and build again.
ALTER TABLE audit_log ADD COLUMN IF NOT EXISTS trace_id uuid;

CREATE INDEX IF NOT EXISTS audit_log_workspace_time_idx ON audit_log (workspace_id, occurred_at, id);
CREATE INDEX IF NOT EXISTS audit_log_workspace_action_idx ON audit_log (workspace_id, action, id);
CREATE INDEX IF NOT EXISTS audit_log_workspace_actor_idx ON audit_log (workspace_id, actor_id, id);
CREATE INDEX IF NOT EXISTS audit_log_workspace_object_idx ON audit_log (workspace_id, object_id, id);
CREATE INDEX IF NOT EXISTS audit_log_request_idx ON audit_log (request_id) WHERE request_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS audit_log_occurred_at_idx ON audit_log (occurred_at);

-- The guard. A row is never updated. A row is deleted only while the transaction-local setting
-- `zirv_flow.audit_purge` is 'on', and only `audit_purge` sets it: it turns the setting on, deletes
-- one batch and turns it off again before it returns, so no later statement of the same
-- transaction inherits it. Nothing in the application issues SET or set_config for that name; the
-- store's only caller is `audit_purge`. TRUNCATE is always refused.
CREATE FUNCTION audit_log_guard() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' AND current_setting('zirv_flow.audit_purge', true) = 'on' THEN
        RETURN OLD;
    END IF;
    RAISE EXCEPTION 'audit_log is append-only (% refused)', TG_OP
        USING ERRCODE = 'insufficient_privilege';
END;
$$;

CREATE TRIGGER audit_log_append_only
    BEFORE UPDATE OR DELETE ON audit_log
    FOR EACH ROW EXECUTE FUNCTION audit_log_guard();

CREATE TRIGGER audit_log_no_truncate
    BEFORE TRUNCATE ON audit_log
    FOR EACH STATEMENT EXECUTE FUNCTION audit_log_guard();

-- The only deleter: removes up to `batch` of the oldest rows before `cutoff` and returns how many.
-- SECURITY DEFINER with a pinned search path, so a role that is later denied DELETE on the table
-- can still be granted EXECUTE on this function alone.
CREATE FUNCTION audit_purge(cutoff timestamptz, batch int) RETURNS bigint
    LANGUAGE plpgsql SECURITY DEFINER SET search_path FROM CURRENT AS $$
DECLARE
    purged bigint;
BEGIN
    IF batch IS NULL OR batch < 1 THEN
        RAISE EXCEPTION 'audit_purge batch must be at least 1';
    END IF;
    PERFORM set_config('zirv_flow.audit_purge', 'on', true);
    DELETE FROM audit_log
    WHERE id IN (SELECT id FROM audit_log WHERE occurred_at < cutoff ORDER BY occurred_at, id LIMIT batch);
    GET DIAGNOSTICS purged = ROW_COUNT;
    PERFORM set_config('zirv_flow.audit_purge', 'off', true);
    RETURN purged;
END;
$$;
