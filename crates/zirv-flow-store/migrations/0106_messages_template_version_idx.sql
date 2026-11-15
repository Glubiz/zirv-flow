-- The per-version stats of a template (`templates::version_stats`) count the non-test messages of
-- one `(template, version)`. `messages_template_idx` is `(workspace_id, template_id, created_at
-- DESC)`, so the version was a filter on the heap: every message of the template was fetched to
-- read one version of it (200 ms and 39,782 buffers on 1.1M rows of one template, version 1 being
-- 20,000 of them, and linear in the template's history).
--
-- This index leads with the same two columns and adds the version, so the read visits only the
-- rows of the version. Test sends are left out like the read leaves them out. The template
-- "most used" read of a later change shares it by its leading columns.
--
-- Write cost: `messages` is the hottest table. Every non-test message adds one entry here at its
-- insert, and the completing upsert (`insert_message` on conflict) rewrites it, because that
-- update is not heap-only already (`status` is indexed). The before and after numbers of the
-- insert-and-complete path are in `docs/operations/upgrade.md`.
--
-- UPGRADE COST: the build takes SHARE on `messages` until the migration commits, which blocks
-- every message insert and update for the build (a stopped upgrade runs it with no writer); it
-- scans the table once. Reads go on. Budget the build at the installation's message count.
CREATE INDEX messages_template_version_idx ON messages (workspace_id, template_id, template_version)
    WHERE NOT test;
