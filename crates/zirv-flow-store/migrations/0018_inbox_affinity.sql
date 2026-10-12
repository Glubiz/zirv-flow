-- Sharded ingest drain: rows of one record key share a bucket, so concurrent drain shards
-- each take their own buckets and still apply a key's rows in id order.

ALTER TABLE ingest_inbox
    ADD COLUMN affinity smallint NULL CHECK (affinity BETWEEN 0 AND 63);

COMMENT ON COLUMN ingest_inbox.affinity IS
    'The bucket of the row''s record key that the drain shards by; NULL marks a row appended before this migration.';
