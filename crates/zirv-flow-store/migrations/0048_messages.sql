-- The message log of the send pipeline (architecture 10.1) and the suppression scope (OQ-19).
--
-- `messages` is person-scoped: its `person_id` column is found by the erasure scan, and erasing a
-- person deletes their rows (`DELETIONS` in the store). It holds no address: the address is read
-- from the person's trait or identity when a message is sent and is never stored. On a merge the
-- rows stay on the person they were written for, as enrollments do (see `merge.rs`); a read of
-- the log follows `person_merges`.

CREATE TABLE messages (
    -- A v7 id minted when `flow.step` enqueues the send. It is the idempotency key passed to the
    -- destination on every attempt of the message, so a retried job never sends twice.
    id                 uuid PRIMARY KEY,
    workspace_id       uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    person_id          uuid NOT NULL REFERENCES people (id) ON DELETE CASCADE,
    -- The enrollment and node that caused the send. The enrollment is not a key: an enrollment is
    -- erased with the person, and the message log is read on its own.
    enrollment_id      uuid NOT NULL,
    flow_id            uuid,
    flow_version       integer,
    node_id            text NOT NULL,
    template_id        uuid NOT NULL,
    -- The version that was rendered; null when no published version could be read.
    template_version   integer,
    destination_id     uuid NOT NULL,
    sender_identity_id uuid,
    category           text CHECK (category IN ('marketing', 'transactional')),
    channel            text NOT NULL DEFAULT 'email' CHECK (channel IN ('email')),
    topic_id           uuid,
    -- The message's consent coordinates besides the topic: dimension key to value key.
    dimensions         jsonb NOT NULL DEFAULT '{}'::jsonb,
    status             text NOT NULL CHECK (status IN ('sent', 'skipped', 'failed')),
    -- Why a message was skipped or failed: a fixed code, never a value from the person or the
    -- destination's answer.
    reason             text,
    -- What decided: `recorded_consent`, `topic_default` or `none`; later the consent policy rule.
    deciding_rule      text,
    -- One entry per gate that ran, in order: {"gate", "result", ...}. Ids and codes only.
    gate_results       jsonb NOT NULL DEFAULT '[]'::jsonb,
    -- A message captured by test mode: nothing was sent and nothing is written, so this is false
    -- for every stored row until test sends (#224) store theirs.
    test               boolean NOT NULL DEFAULT false,
    -- How many delivery attempts the destination saw.
    attempts           integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    created_at         timestamptz NOT NULL DEFAULT now(),
    completed_at       timestamptz,
    UNIQUE (workspace_id, id),
    CHECK ((status = 'sent') = (reason IS NULL))
);

-- The message log of a person and the cooldown reads: newest first.
CREATE INDEX messages_person_idx ON messages (workspace_id, person_id, created_at DESC);
CREATE INDEX messages_template_idx ON messages (workspace_id, template_id, created_at DESC);

-- The per-destination queue depth and oldest-item lag of the leader's gauges read the waiting
-- `delivery.send` jobs by destination.
CREATE INDEX jobs_delivery_send_idx ON jobs ((payload ->> 'destination_id'), run_at)
    WHERE kind = 'delivery.send' AND state = 'ready';

-- Suppression scope (OQ-19). A row is per workspace (the default, every existing row) or
-- organization-wide, which has no workspace: a hard bounce means the address does not exist for
-- any brand. The primary key (workspace, hash) cannot key a row with no workspace, so it is
-- replaced by one unique index per scope. `is_suppressed` reads both scopes in one statement
-- over the two indexes.
ALTER TABLE suppressions ADD COLUMN scope text NOT NULL DEFAULT 'workspace'
    CHECK (scope IN ('workspace', 'organization'));
ALTER TABLE suppressions DROP CONSTRAINT suppressions_pkey;
ALTER TABLE suppressions ALTER COLUMN workspace_id DROP NOT NULL;
ALTER TABLE suppressions ADD CONSTRAINT suppressions_scope_workspace_check
    CHECK ((scope = 'workspace') = (workspace_id IS NOT NULL));
CREATE UNIQUE INDEX suppressions_workspace_hash_idx ON suppressions (workspace_id, identifier_hash)
    WHERE scope = 'workspace';
CREATE UNIQUE INDEX suppressions_organization_hash_idx ON suppressions (identifier_hash)
    WHERE scope = 'organization';
