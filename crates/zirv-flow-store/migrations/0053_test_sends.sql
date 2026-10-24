-- The test-send log and the test-send allowlist (architecture 11.7, #224).
--
-- A test send writes no `messages` row (supervisor ruling 1d3048b4): `0048_messages.sql` still
-- says `messages.test` is false for every stored row, and that stays true. Applied migrations are
-- never edited, because sqlx checks their checksums; this one only adds tables.
--
-- Neither table stores an address. `test_sends.recipient_hash` is the keyed identifier HMAC the
-- `suppressions` table is keyed by, and the allowlist holds the same hash plus a masked hint (the
-- first character, `***`, then `@domain`). The log is not person-scoped: it names no person, so the
-- erasure scan has no column to find. Erasing a person deletes the rows whose `recipient_hash` is
-- one of the person's erased identifier hashes; the allowlist is admin configuration that an
-- admin added, holds only a hash and a hint, and is not touched by a person's erasure. Rows older
-- than 13 months are deleted by the `events.retention` job.

CREATE TABLE test_sends (
    -- Also the idempotency key at the destination.
    id                  uuid PRIMARY KEY,
    workspace_id        uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    template_id         uuid NOT NULL,
    -- The published version that was sent; null for the draft.
    template_version    integer CHECK (template_version IS NULL OR template_version > 0),
    -- The user who asked; null for an API key.
    member_id           uuid REFERENCES users (id) ON DELETE SET NULL,
    destination_id      uuid NOT NULL,
    sender_identity_id  uuid NOT NULL,
    recipient_hash      bytea NOT NULL CHECK (octet_length(recipient_hash) = 32),
    -- The workspace member the test went to; null for an allowlisted address.
    recipient_member_id uuid REFERENCES users (id) ON DELETE SET NULL,
    -- Whose values rendered the template. The person's id is not stored.
    render_context      text NOT NULL CHECK (render_context IN ('person', 'sample')),
    outcome             text NOT NULL DEFAULT 'queued'
                        CHECK (outcome IN ('queued', 'sent', 'suppressed', 'sender_unverified',
                                           'render_failed', 'unconfirmed', 'failed')),
    -- A fixed code, never a value from the destination's answer.
    reason              text,
    created_at          timestamptz NOT NULL DEFAULT now(),
    completed_at        timestamptz,
    CHECK ((outcome = 'queued') = (completed_at IS NULL))
);

CREATE INDEX test_sends_list_idx ON test_sends (workspace_id, created_at DESC, id DESC);
CREATE INDEX test_sends_member_idx ON test_sends (workspace_id, member_id, created_at);
CREATE INDEX test_sends_recipient_idx ON test_sends (workspace_id, recipient_hash);
CREATE INDEX test_sends_age_idx ON test_sends (created_at);

CREATE TABLE test_send_allowlist (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    address_hash bytea NOT NULL CHECK (octet_length(address_hash) = 32),
    -- The first character, `***`, then `@domain`.
    display_hint text NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, address_hash)
);
