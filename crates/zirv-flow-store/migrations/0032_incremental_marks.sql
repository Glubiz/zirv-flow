-- Incremental re-evaluation (architecture sections 7.1 and 8.4). A dirty mark now says what
-- changed and why: the change classes OR-ed over the marks it coalesced (the `ChangeClasses`
-- bits of the core crate) and the trace and cause of the latest change, which the entries and
-- exits it causes continue.
ALTER TABLE person_changes
    ADD COLUMN classes    smallint NOT NULL DEFAULT 0,
    ADD COLUMN trace_id   uuid,
    ADD COLUMN cause_kind text
        CHECK (cause_kind IN ('event', 'trait_change', 'schedule', 'manual', 'api', 'definition_change', 'sync_run')),
    ADD COLUMN cause_id   text;

-- A mark made before this migration does not know what changed: every class.
UPDATE person_changes SET classes = 63;

-- The claim takes the oldest marks of a workspace, and the leader's safety net reads the oldest.
CREATE INDEX person_changes_changed_idx ON person_changes (workspace_id, changed_at, person_id);
