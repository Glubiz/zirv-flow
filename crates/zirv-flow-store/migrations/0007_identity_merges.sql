-- Identity merges and conflict review (architecture section 7.3): the provenance of every
-- merge, and the identity and resolution a conflict needs to be reviewed.

CREATE TABLE person_merges (
    id                  uuid PRIMARY KEY,
    workspace_id        uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    survivor_person_id  uuid NOT NULL REFERENCES people (id),
    merged_person_id    uuid NOT NULL REFERENCES people (id),
    -- What moved from the merged person to the survivor.
    identity_ids        uuid[] NOT NULL DEFAULT '{}',
    source_record_ids   uuid[] NOT NULL DEFAULT '{}',
    trace_id            uuid NOT NULL,
    created_at          timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX person_merges_survivor_idx ON person_merges (workspace_id, survivor_person_id);
CREATE INDEX person_merges_merged_idx ON person_merges (workspace_id, merged_person_id);

-- The shared identity the conflict is about. Null on conflicts recorded before merges existed.
ALTER TABLE identity_conflicts
    ADD COLUMN identity_id uuid REFERENCES identities (id),
    ADD COLUMN resolution text
        CHECK (resolution IN ('merged', 'kept_separate', 'identifier_moved'));

CREATE INDEX identity_conflicts_other_person_idx
    ON identity_conflicts (workspace_id, other_person_id);
-- Newest first, for the review list.
CREATE INDEX identity_conflicts_review_idx
    ON identity_conflicts (workspace_id, created_at DESC, id DESC);
