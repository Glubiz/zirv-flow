-- The draft revision a draft test rendered, so the publish review can find "the last test of this
-- draft". Null for a version test and for rows made before this column. It is a counter of saves,
-- not content, so it holds no person data.
ALTER TABLE test_sends
    ADD COLUMN draft_revision integer CHECK (draft_revision IS NULL OR draft_revision > 0);

CREATE INDEX test_sends_draft_idx ON test_sends (workspace_id, template_id, draft_revision)
    WHERE draft_revision IS NOT NULL;
