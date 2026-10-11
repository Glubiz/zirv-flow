-- CSV imports (architecture sections 7.1, 7.2 and 16.3): the uploaded file kept in chunks, the
-- mapping, the run's checkpoint and counters, and the rows that were refused.
--
-- No column here refers to a person: the import's records carry their own `source_records`
-- keys. The one place a person's data can sit is `import_rejects.raw`, the refused row's
-- cells, which only admins read and which is deleted 30 days after it was written; the file
-- itself is deleted 7 days after the import ends.

CREATE TABLE imports (
    id               uuid PRIMARY KEY,
    workspace_id     uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name             text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 128),
    -- The source and stream the import's records are written under; keep them stable so a
    -- re-run of the same file finds its records and changes nothing.
    source           text NOT NULL DEFAULT 'csv' CHECK (source ~ '^[a-z0-9_-]{1,64}$'),
    stream           text NOT NULL CHECK (stream ~ '^[a-z0-9_-]{1,64}$'),
    file_name        text NOT NULL,
    file_sha256      bytea NOT NULL CHECK (octet_length(file_sha256) = 32),
    size_bytes       bigint NOT NULL CHECK (size_bytes >= 0),
    delimiter        text NOT NULL CHECK (delimiter IN (',', ';', E'\t')),
    -- The column names, in file order.
    header           jsonb NOT NULL,
    mapping          jsonb,
    status           text NOT NULL DEFAULT 'uploaded' CHECK (status IN
        ('uploaded', 'mapped', 'dry_running', 'dry_run_done', 'running', 'succeeded', 'failed',
         'cancelled')),
    -- Data rows, the header not counted.
    rows_total       bigint NOT NULL CHECK (rows_total >= 0),
    -- The checkpoint: rows the current phase has finished, rejected ones included.
    rows_done        bigint NOT NULL DEFAULT 0,
    rows_applied     bigint NOT NULL DEFAULT 0,
    rows_unchanged   bigint NOT NULL DEFAULT 0,
    rows_rejected    bigint NOT NULL DEFAULT 0,
    reject_threshold double precision NOT NULL DEFAULT 0.05
        CHECK (reject_threshold >= 0 AND reject_threshold <= 1),
    -- Why a failed import failed: a code, never a value.
    error_code       text,
    created_by       uuid REFERENCES users (id),
    trace_id         uuid NOT NULL,
    created_at       timestamptz NOT NULL DEFAULT now(),
    updated_at       timestamptz NOT NULL DEFAULT now(),
    -- When the run was started; the records are observed at this time.
    started_at       timestamptz,
    finished_at      timestamptz
);

-- Newest first (ids are time-ordered), for the history.
CREATE INDEX imports_workspace_idx ON imports (workspace_id, id DESC);

-- The file, 1 MiB per chunk, so separate `serve` and `worker` processes need no shared disk.
-- The upload writes the chunks before the import row in one transaction.
CREATE TABLE import_chunks (
    import_id    uuid NOT NULL REFERENCES imports (id) ON DELETE CASCADE
        DEFERRABLE INITIALLY DEFERRED,
    seq          integer NOT NULL CHECK (seq >= 0),
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    data         bytea NOT NULL,
    PRIMARY KEY (import_id, seq)
);

CREATE INDEX import_chunks_workspace_idx ON import_chunks (workspace_id, import_id);

CREATE TABLE import_rejects (
    import_id    uuid NOT NULL REFERENCES imports (id) ON DELETE CASCADE,
    -- The row as a spreadsheet numbers it: the header is row 1.
    row_number   bigint NOT NULL CHECK (row_number >= 2),
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    phase        text NOT NULL CHECK (phase IN ('dry_run', 'run')),
    -- [{column, reason}]: codes only.
    errors       jsonb NOT NULL,
    -- The row's cells. Personal data: admins only, deleted after 30 days.
    raw          jsonb NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (import_id, row_number)
);

CREATE INDEX import_rejects_workspace_idx ON import_rejects (workspace_id, import_id);
-- The retention delete runs across workspaces, oldest first.
CREATE INDEX import_rejects_created_idx ON import_rejects (created_at);
