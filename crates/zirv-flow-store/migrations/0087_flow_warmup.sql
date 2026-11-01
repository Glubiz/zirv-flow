-- Flow warm-up (architecture 9.4): for a window after a flow starts taking enrollments, entries
-- are admitted along a ramp and the rest wait for their turn. The setting lives on the flow, not
-- in the versioned graph, so toggling it and opening the flow early need no publish.
--
-- Locking: ADD COLUMN with a constant default is a catalog change in PostgreSQL 11+, and the
-- nullable columns have no default, so no rows are rewritten. A flow published before this
-- migration has no start, so it is not warming up.
ALTER TABLE flows
    ADD COLUMN warmup_enabled boolean NOT NULL DEFAULT true,
    ADD COLUMN warmup_seconds integer NOT NULL DEFAULT 300
        CHECK (warmup_seconds BETWEEN 60 AND 86400),
    ADD COLUMN warmup_started_at timestamptz,
    ADD COLUMN warmup_opened_at timestamptz;

-- An entry that is not admitted yet. `admission_offset` is the entry's stable place in the
-- window, as a fraction of 2^53; ids only, never a trait value.
CREATE TABLE flow_warmup_waiting (
    workspace_id     uuid NOT NULL,
    flow_id          uuid NOT NULL,
    person_id        uuid NOT NULL REFERENCES people (id) ON DELETE CASCADE,
    occurrence_id    uuid NOT NULL,
    flow_version     integer NOT NULL CHECK (flow_version > 0),
    trigger          text NOT NULL,
    admission_offset bigint NOT NULL CHECK (admission_offset >= 0 AND admission_offset < 9007199254740992),
    trace_id         uuid NOT NULL,
    cause_kind       text NOT NULL,
    cause_id         text NOT NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (flow_id, person_id, occurrence_id),
    FOREIGN KEY (workspace_id, flow_id) REFERENCES flows (workspace_id, id) ON DELETE CASCADE
);
-- A person's erasure deletes their waiting entries.
CREATE INDEX flow_warmup_waiting_person_idx ON flow_warmup_waiting (person_id);
