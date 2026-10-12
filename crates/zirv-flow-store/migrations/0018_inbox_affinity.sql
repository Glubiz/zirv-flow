-- Sharded ingest drain: rows of one record key share a bucket, so concurrent drain shards
-- each take their own buckets and still apply a key's rows in id order.

ALTER TABLE ingest_inbox
    ADD COLUMN affinity smallint NULL CHECK (affinity BETWEEN 0 AND 63);

COMMENT ON COLUMN ingest_inbox.affinity IS
    'The bucket of the row''s record key that the drain shards by; NULL means not yet bucketed: appended before 0018, or by a pre-0.29 serve during a rolling upgrade.';

-- A shard takes the oldest rows of each of its buckets, and the drain finds unbucketed rows
-- with `affinity IS NULL`; a btree indexes NULLs, so one index serves both.
CREATE INDEX ingest_inbox_affinity_idx ON ingest_inbox (workspace_id, affinity, id);
