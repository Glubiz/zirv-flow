-- The Kafka source (architecture sections 7.1, 7.5 and 7.6): the `stream` sync mode and the
-- per-partition offsets of a consumer. Nothing in this file holds a record value, a record key
-- or a person.

-- A stream that never ends is read by a consumer, not by a pull run.
ALTER TABLE streams DROP CONSTRAINT streams_sync_mode_check;
ALTER TABLE streams ADD CONSTRAINT streams_sync_mode_check
    CHECK (sync_mode IN ('full_refresh', 'incremental', 'stream'));
ALTER TABLE sync_runs DROP CONSTRAINT sync_runs_sync_mode_check;
ALTER TABLE sync_runs ADD CONSTRAINT sync_runs_sync_mode_check
    CHECK (sync_mode IN ('full_refresh', 'incremental', 'stream'));

-- Where a consumer resumes, one row per partition of the stream's topic (the stream name is the
-- topic). A missing row means "earliest". The offsets are written in the transaction that
-- applies the batch they cover, so a restart neither skips nor repeats a committed batch.
CREATE TABLE stream_offsets (
    workspace_id   uuid NOT NULL,
    stream_id      uuid NOT NULL,
    partition      integer NOT NULL CHECK (partition >= 0),
    -- The next offset to read.
    next_offset    bigint NOT NULL CHECK (next_offset >= 0),
    -- The log's end when it was last looked at, and when: their difference to `next_offset`
    -- is the partition's lag.
    high_watermark bigint CHECK (high_watermark >= 0),
    observed_at    timestamptz,
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (stream_id, partition),
    FOREIGN KEY (workspace_id, stream_id) REFERENCES streams (workspace_id, id) ON DELETE CASCADE
);
