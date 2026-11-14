-- A source-side deletion used to tombstone the ledger row (`deleted_at`) and keep its `traits`
-- and `identifiers`, so a removed record's personal data stayed in the table and in exports
-- (`people/apply.rs`, `remove_contribution_in`). The code now empties them in the tombstone.
-- This clears the tombstones written before. Nothing reads a tombstone's data: a revived
-- record starts empty, and `content_hash` of a tombstone never matches (the unchanged check
-- requires `deleted_at IS NULL`). `content_hash` is NOT NULL, so it becomes the empty value.
--
-- Data only; no index covers these rows (`source_records_traits_idx` lists live records), so
-- the statement writes only the tombstone heap rows it changes, and none once it has run.
UPDATE source_records
SET traits = '{}'::jsonb, identifiers = '[]'::jsonb, content_hash = ''::bytea
WHERE deleted_at IS NOT NULL
  AND (traits <> '{}'::jsonb OR identifiers <> '[]'::jsonb OR content_hash <> ''::bytea);
