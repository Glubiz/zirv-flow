-- Flow drafts for the assistant (architecture 13.2): a draft may now be a flow graph with its
-- settings. A flow draft names the flow whose ordinary draft it would replace in `flow_id` and has
-- no source, stream or segment. The draft is not a flow version and not the flow's own draft:
-- accepting it copies it into the flow's draft in one transaction, and never publishes.
ALTER TABLE ai_drafts ADD COLUMN flow_id uuid;

ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_flow_fkey
    FOREIGN KEY (workspace_id, flow_id) REFERENCES flows (workspace_id, id) ON DELETE CASCADE;

ALTER TABLE ai_drafts DROP CONSTRAINT ai_drafts_kind_check;
ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_kind_check CHECK (kind IN ('mapping', 'manifest', 'segment', 'flow'));

-- Only a flow draft names a flow, and a flow draft always does; it names nothing else.
ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_flow_target_check
    CHECK ((kind = 'flow') = (flow_id IS NOT NULL));
ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_flow_no_other_target_check
    CHECK (kind <> 'flow' OR (source_id IS NULL AND stream_name IS NULL AND segment_id IS NULL));

CREATE INDEX ai_drafts_flow_idx ON ai_drafts (workspace_id, flow_id) WHERE flow_id IS NOT NULL;
