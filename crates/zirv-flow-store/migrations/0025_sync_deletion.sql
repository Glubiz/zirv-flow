-- Deletion for synced sources (architecture sections 7.2 and 7.4): the sweep that removes the
-- live records a complete read did not see, reconciliation runs, the mass-delete guard and
-- record-linked events. Nothing in this file holds a record value, and nothing is backfilled.

-- An event may name the ledger record it came from, so removing the record removes its events.
-- No foreign key: `events` is partitioned and a ledger row is only ever tombstoned. Null for the
-- first-party events. Created on the partitioned parent, so every partition, existing and
-- future, has the index.
ALTER TABLE events ADD COLUMN source_record_id uuid;
CREATE INDEX events_source_record_idx ON events (workspace_id, source_record_id)
    WHERE source_record_id IS NOT NULL;

-- What a run is: a normal sync, or a reconciliation, which reads only to learn which records
-- still exist and then sweeps. Only an incremental stream reconciles.
ALTER TABLE sync_runs
    ADD COLUMN run_kind text NOT NULL DEFAULT 'sync' CHECK (run_kind IN ('sync', 'reconcile'));

-- The UTC cron of a stream's reconciliation, mirrored to a `schedules` row; null for none.
ALTER TABLE streams ADD COLUMN reconcile_schedule text;

-- The largest share of a stream's live records one sweep may remove. A sweep over it removes
-- nothing and fails the run; 1 turns the guard off.
ALTER TABLE streams ADD COLUMN sweep_limit double precision NOT NULL DEFAULT 0.5
    CHECK (sweep_limit > 0 AND sweep_limit <= 1);

-- A stream's reconciliation cron, next to the source's own schedule row, which has no stream.
-- Deleting the stream or its source deletes it.
ALTER TABLE schedules
    ADD COLUMN stream_id uuid REFERENCES streams (id) ON DELETE CASCADE;
CREATE UNIQUE INDEX schedules_stream_idx ON schedules (stream_id) WHERE stream_id IS NOT NULL;

-- The sweep's keyset scan over the live records of one stream.
CREATE INDEX source_records_live_stream_idx ON source_records (workspace_id, source, stream, id)
    WHERE deleted_at IS NULL;
