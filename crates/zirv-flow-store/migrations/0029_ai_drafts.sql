-- Assistant drafts (architecture 13.2, 15 `ai/drafts`): a mapping or manifest the model drafted
-- and the validator approved, kept until it is accepted into an editor or expires. A draft that
-- never validated keeps no content: raw model output is never stored. Content is an artifact
-- like a saved mapping; it holds no secret (a manifest names credential fields, never values).
CREATE TABLE ai_drafts (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id      uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    kind         text NOT NULL CHECK (kind IN ('mapping', 'manifest')),
    source_id    uuid,
    stream_name  text CHECK (char_length(stream_name) BETWEEN 1 AND 256),
    status       text NOT NULL CHECK (status IN ('valid', 'invalid', 'accepted')),
    content      jsonb,
    -- SHA-256 of the saved mapping or manifest the draft was compared with; null when none.
    base_digest  bytea CHECK (octet_length(base_digest) = 32),
    created_at   timestamptz NOT NULL DEFAULT now(),
    accepted_at  timestamptz,
    expires_at   timestamptz NOT NULL,
    FOREIGN KEY (workspace_id, source_id) REFERENCES sources (workspace_id, id) ON DELETE CASCADE,
    CHECK ((status = 'invalid') = (content IS NULL)),
    CHECK ((status = 'accepted') = (accepted_at IS NOT NULL)),
    CHECK (kind <> 'mapping' OR (source_id IS NOT NULL AND stream_name IS NOT NULL)),
    CHECK (kind <> 'manifest' OR stream_name IS NULL)
);
CREATE INDEX ai_drafts_expires_idx ON ai_drafts (expires_at);
CREATE INDEX ai_drafts_source_idx ON ai_drafts (workspace_id, source_id) WHERE source_id IS NOT NULL;
