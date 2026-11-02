-- The merge group of a person (`people/timeline.rs`, #397) finds the people merged into a
-- survivor by `merged_into`, and so does a merge that moves its losers' followers. No index
-- covered that column, so each lookup scanned every person of the table.
--
-- `people_merged_into_idx` holds only the merged people (a small part of the table), led by the
-- workspace like every lookup that uses it. The lookups state `merged_into IS NOT NULL` or an
-- equality on it, which lets the planner use the partial index.
--
-- The build takes a SHARE lock on `people` until it is done, which blocks every write to it. A
-- populated deployment builds the index first, by hand, with the statement below; the migration
-- then finds it and does nothing. A failed concurrent build leaves an invalid index of its name:
-- drop it and build it again before upgrading.
--
--     CREATE INDEX CONCURRENTLY people_merged_into_idx ON people (workspace_id, merged_into)
--         WHERE merged_into IS NOT NULL;
CREATE INDEX IF NOT EXISTS people_merged_into_idx ON people (workspace_id, merged_into)
    WHERE merged_into IS NOT NULL;
