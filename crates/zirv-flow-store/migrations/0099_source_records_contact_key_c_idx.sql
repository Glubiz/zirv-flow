-- The contact key-prefix search (`search_contacts`, `ContactSearch::KeyPrefix`) filtered with
-- `starts_with(record_key, $2) ORDER BY record_key LIMIT n`. `starts_with` is not an index
-- condition, and the unique key's `record_key` sorts in the database collation, so a prefix
-- cannot become a range on it: the planner walked the stream in key order, fetching each row
-- from the heap, until it had `n` matches (1M contacts: a prefix that matched a few keys or none
-- read the whole stream).
--
-- Under `COLLATE "C"` a prefix is a byte range, so this index serves `record_key >= prefix AND
-- record_key < next prefix ORDER BY record_key LIMIT n` in index order, whatever the prefix
-- matches. It lists only the live native contacts, the only rows the search reads, and its
-- predicate is the one the query states.
--
-- The search therefore returns its matches in byte order (`B1` before `a1`) and no longer in the
-- database collation, as `list_contacts` still does.
--
-- UPGRADE COST: the build takes SHARE on `source_records` until the migration commits, which
-- blocks every write to the ledger (the drain's profile records, sync and Kafka units, imports,
-- contacts) for the build; reads go on. `docs/operations/upgrade.md` gives its measured
-- duration. Plan the upgrade for a quiet window.
CREATE INDEX source_records_native_contact_key_idx
    ON source_records (workspace_id, (record_key COLLATE "C"))
    WHERE source = 'native' AND stream = 'contacts' AND deleted_at IS NULL;
