-- Image assets (architecture 11.3): the uploaded bytes of an image, per workspace, and the
-- published versions that use it.
--
-- Neither table refers to a person: an asset is a file a marketer uploaded, `created_by` is a
-- user, and a reference names a template, theme or snippet version. The erasure scan and the
-- identity merge have nothing to do here.

CREATE TABLE assets (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    -- The SHA-256 of the bytes; the public URL is `/a/<hex>.<ext>`.
    sha256       bytea NOT NULL CHECK (octet_length(sha256) = 32),
    ext          text NOT NULL CHECK (ext IN ('png', 'jpg', 'gif', 'webp')),
    content_type text NOT NULL CHECK (content_type IN
        ('image/png', 'image/jpeg', 'image/gif', 'image/webp')),
    -- Empty once the asset is deleted, so the space is freed.
    bytes        bytea NOT NULL,
    -- The size the bytes had; counts against the workspace quota while the asset is live.
    size_bytes   bigint NOT NULL CHECK (size_bytes > 0),
    width        integer NOT NULL CHECK (width > 0),
    height       integer NOT NULL CHECK (height > 0),
    name         text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    created_by   uuid REFERENCES users (id),
    created_at   timestamptz NOT NULL DEFAULT now(),
    deleted_at   timestamptz
);

-- The same bytes uploaded twice in one workspace are one live row.
CREATE UNIQUE INDEX assets_workspace_sha256_live_idx
    ON assets (workspace_id, sha256) WHERE deleted_at IS NULL;

-- Public serving looks an asset up by hash alone.
CREATE INDEX assets_sha256_live_idx ON assets (sha256) WHERE deleted_at IS NULL;

-- Newest first (ids are time-ordered), for the list.
CREATE INDEX assets_workspace_idx ON assets (workspace_id, id DESC);

-- The published versions that use an asset. Written in the publish transaction of the owner; an
-- asset with any row here cannot be deleted.
CREATE TABLE asset_references (
    asset_id   uuid NOT NULL REFERENCES assets (id),
    owner_kind text NOT NULL CHECK (owner_kind IN ('template', 'theme', 'snippet')),
    owner_id   uuid NOT NULL,
    version    integer NOT NULL CHECK (version > 0),
    PRIMARY KEY (asset_id, owner_kind, owner_id, version)
);
