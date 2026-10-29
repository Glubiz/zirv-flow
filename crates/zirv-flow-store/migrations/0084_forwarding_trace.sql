-- Forwarding decisions follow the originating event (architecture 12.4 and 12.5), even when
-- one destination job delivers rows from several traces. This UUID is not personal data.
-- Existing rows retain NULL and use the destination job's trace until they leave the outbox.
--
-- Locking: ADD COLUMN takes ACCESS EXCLUSIVE on forwarding_outbox. The nullable column has no
-- default or backfill, so PostgreSQL changes the catalog without rewriting or scanning rows.
ALTER TABLE forwarding_outbox ADD COLUMN trace_id uuid;
