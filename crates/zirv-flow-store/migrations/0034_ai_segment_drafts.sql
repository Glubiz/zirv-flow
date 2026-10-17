-- Segment drafts for the assistant (architecture 13.2): a draft may now be a segment definition.
-- A segment draft names the segment it would change in `segment_id` (null for a new segment) and
-- has no source or stream. Mapping and manifest drafts keep their rules, and `segment_id` stays
-- null for them. The draft is not a segment version: accepting it writes nothing to `segments`.
ALTER TABLE ai_drafts ADD COLUMN segment_id uuid;

ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_segment_fkey
    FOREIGN KEY (workspace_id, segment_id) REFERENCES segments (workspace_id, id) ON DELETE CASCADE;

ALTER TABLE ai_drafts DROP CONSTRAINT ai_drafts_kind_check;
ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_kind_check CHECK (kind IN ('mapping', 'manifest', 'segment'));

-- The source and stream rules, one per kind: a mapping needs both, a manifest has no stream, and a
-- segment has neither. Only a segment draft may name a segment.
ALTER TABLE ai_drafts DROP CONSTRAINT ai_drafts_check2;
ALTER TABLE ai_drafts DROP CONSTRAINT ai_drafts_check3;
ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_mapping_target_check
    CHECK (kind <> 'mapping' OR (source_id IS NOT NULL AND stream_name IS NOT NULL));
ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_manifest_target_check
    CHECK (kind <> 'manifest' OR stream_name IS NULL);
ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_segment_target_check
    CHECK (kind = 'segment' OR segment_id IS NULL);
ALTER TABLE ai_drafts
    ADD CONSTRAINT ai_drafts_segment_no_source_check
    CHECK (kind <> 'segment' OR (source_id IS NULL AND stream_name IS NULL));

CREATE INDEX ai_drafts_segment_idx ON ai_drafts (workspace_id, segment_id) WHERE segment_id IS NOT NULL;
