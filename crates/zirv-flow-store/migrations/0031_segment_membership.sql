-- Segment membership and the full refresh (architecture sections 6, 8.3 and 8.4). A segment's
-- members are rows here, one open row per person; a refresh recomputes the set, diffs it against
-- the open rows and writes the entries and exits. The columns on `segments` are written only by a
-- refresh.

ALTER TABLE segments
    -- The version the members were last computed from; null before the first refresh.
    ADD COLUMN computed_version integer,
    -- The captured clock time of the last refresh.
    ADD COLUMN computed_at      timestamptz,
    -- The open rows of segment_members, kept in step by the refresh, a merge and a deletion.
    ADD COLUMN member_count     integer NOT NULL DEFAULT 0 CHECK (member_count >= 0);

CREATE TABLE segment_members (
    id               bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    workspace_id     uuid NOT NULL,
    segment_id       uuid NOT NULL,
    person_id        uuid NOT NULL REFERENCES people (id),
    -- The version that entered the person, and the version whose refresh closed the row.
    version          integer NOT NULL CHECK (version > 0),
    entered_at       timestamptz NOT NULL,
    entered_trace_id uuid NOT NULL,
    entered_cause    text NOT NULL
        CHECK (entered_cause IN ('initial', 'definition_change', 'data_change', 'time', 'manual')),
    exited_version   integer CHECK (exited_version > 0),
    exited_at        timestamptz,
    exited_trace_id  uuid,
    exited_cause     text
        CHECK (exited_cause IN ('initial', 'definition_change', 'data_change', 'time', 'manual')),
    -- A row is open or fully closed.
    CHECK ((exited_at IS NULL) = (exited_trace_id IS NULL) AND (exited_at IS NULL) = (exited_cause IS NULL)),
    FOREIGN KEY (workspace_id, segment_id) REFERENCES segments (workspace_id, id) ON DELETE CASCADE
);

-- One open row per segment and person. It also serves the compiled `segment` node, whose
-- predicate is workspace_id, person_id, segment_id and exited_at IS NULL, and the member pages.
CREATE UNIQUE INDEX segment_members_open_idx ON segment_members (segment_id, person_id)
    WHERE exited_at IS NULL;
-- The person's rows: merge, deletion and export.
CREATE INDEX segment_members_person_idx ON segment_members (workspace_id, person_id);
-- Newest first, for the history pages: entries by their time, exits by theirs.
CREATE INDEX segment_members_entered_idx ON segment_members (segment_id, entered_at DESC, id DESC);
CREATE INDEX segment_members_exited_idx ON segment_members (segment_id, exited_at DESC, id DESC)
    WHERE exited_at IS NOT NULL;

-- A segment's cadence, mirrored to a schedule the leader fires, as a source's cron is. Deleting
-- the segment deletes it.
ALTER TABLE schedules
    ADD COLUMN segment_id uuid REFERENCES segments (id) ON DELETE CASCADE;
CREATE UNIQUE INDEX schedules_segment_idx ON schedules (segment_id) WHERE segment_id IS NOT NULL;
