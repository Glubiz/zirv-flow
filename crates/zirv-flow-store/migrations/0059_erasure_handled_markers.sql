-- The "destination D handled person P" fact that outlives its evidence (architecture 16.3, "As
-- built (erase calls, #227)" and the handled markers of #283).
--
-- An erasure plans an erase call for every destination that handled the person, read from the
-- person's messages, forwarding outbox rows, `sync.*` events and `delivery.sync` jobs. Retention
-- removes the first three when they age out. Each retention purge that removes such rows writes a
-- marker for the (person, destination) pairs of the rows it removes, in its own statement; the
-- hot paths (a message insert, an outbox insert, a sync outcome) write none. The erasure reads the
-- markers with the other evidence and deletes them in its transaction, after it has planned.
--
-- The person key cascades, so a marker can never outlive its person. The destination key cascades
-- like every other table that names a destination.

-- The composite key the marker's person reference needs; `id` alone is already unique.
ALTER TABLE people ADD CONSTRAINT people_workspace_id_id_key UNIQUE (workspace_id, id);

CREATE TABLE erasure_handled_markers (
    workspace_id     uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    person_id        uuid NOT NULL,
    destination_id   uuid NOT NULL,
    first_handled_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (workspace_id, person_id, destination_id),
    FOREIGN KEY (workspace_id, person_id) REFERENCES people (workspace_id, id)
        ON DELETE CASCADE,
    FOREIGN KEY (workspace_id, destination_id) REFERENCES destinations (workspace_id, id)
        ON DELETE CASCADE
);

CREATE INDEX erasure_handled_markers_destination_idx
    ON erasure_handled_markers (workspace_id, destination_id);
