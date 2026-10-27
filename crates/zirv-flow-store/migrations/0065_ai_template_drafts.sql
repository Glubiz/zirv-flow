-- Template drafts for the assistant (architecture 13.2, 13.3): a draft may now be a template
-- document. A template draft names the template whose ordinary draft it would replace in
-- `template_id` and has no source, stream, segment or flow. It is not a template version and not
-- the template's own draft: accepting it copies it into the template's draft in one transaction,
-- and never publishes. The context is sample-only, so the row holds no person value.
ALTER TABLE ai_drafts ADD COLUMN template_id uuid;

ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_template_fkey
    FOREIGN KEY (workspace_id, template_id) REFERENCES templates (workspace_id, id) ON DELETE CASCADE;

ALTER TABLE ai_drafts DROP CONSTRAINT ai_drafts_kind_check;
ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_kind_check
    CHECK (kind IN ('mapping', 'manifest', 'segment', 'flow', 'template'));

-- Only a template draft names a template, and a template draft always does; it names nothing else.
ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_template_target_check
    CHECK ((kind = 'template') = (template_id IS NOT NULL));
ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_template_no_other_target_check
    CHECK (kind <> 'template' OR (source_id IS NULL AND stream_name IS NULL
        AND segment_id IS NULL AND flow_id IS NULL));

CREATE INDEX ai_drafts_template_idx ON ai_drafts (workspace_id, template_id) WHERE template_id IS NOT NULL;
