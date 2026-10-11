-- The live event debugger and the event explorer (architecture sections 12.2 and 12.3).

-- Who is watching. A `serve` process with a viewer for a workspace upserts `until` a little
-- ahead of now every few seconds; ingestion writes the feed only while `until` is in the
-- future, so a workspace nobody watches pays no extra write.
CREATE TABLE debugger_watch (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    until        timestamptz NOT NULL
);

-- One row per event outcome while the workspace is watched. UNLOGGED: it is a debug aid, and
-- losing it in a crash is acceptable. It holds the REDACTED properties only (the same values
-- the event or reject keeps); masking for the viewer happens when a viewer reads.
CREATE UNLOGGED TABLE debugger_feed (
    workspace_id   uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    seq            bigint GENERATED ALWAYS AS IDENTITY,
    at             timestamptz NOT NULL DEFAULT now(),
    source         text NOT NULL,
    name           text,
    -- accepted, flagged, unplanned, rejected or duplicate.
    outcome        text NOT NULL
        CHECK (outcome IN ('accepted', 'flagged', 'unplanned', 'rejected', 'duplicate')),
    -- [{property, reason}]: names and codes only, never values.
    reasons        jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- The static reject code of a rejected event, such as validation_failed.
    reason         text,
    properties     jsonb NOT NULL DEFAULT '{}'::jsonb,
    -- The published schema version the event was judged against, so a viewer's masking can
    -- follow it; absent when there was none.
    schema_version integer,
    event_id       uuid,
    reject_id      uuid,
    trace_id       uuid NOT NULL,
    PRIMARY KEY (workspace_id, seq)
);

-- The leader's upkeep deletes by age.
CREATE INDEX debugger_feed_at_idx ON debugger_feed (at);

-- Explorer queries carry the workspace and a time range, whatever else they filter.
CREATE INDEX events_time_idx ON events (workspace_id, occurred_at DESC);

-- A timestamp filter of the explorer reads a stored property that a flagged event may hold in
-- the wrong shape: a value that is not an RFC 3339 timestamp compares as absent instead of
-- failing the query. PostgreSQL's own special strings (now, today, infinity) and values
-- without a zone are not RFC 3339 and are refused before the cast.
CREATE FUNCTION zf_try_timestamptz(value text) RETURNS timestamptz
LANGUAGE plpgsql STABLE AS $$
BEGIN
    IF value !~ '^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})$' THEN
        RETURN NULL;
    END IF;
    RETURN value::timestamptz;
EXCEPTION WHEN others THEN
    RETURN NULL;
END
$$;

-- The redaction rules in force when a reject was written, so a replay applies only what the
-- payload has not had yet (a hash is bare hex and cannot be told from a value). Null for a
-- reject written before this column existed: nothing is known to have been applied.
ALTER TABLE ingest_rejects ADD COLUMN redaction jsonb;
