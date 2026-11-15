-- The unsubscribe and preference links of a mail name only the message id, and the page read the
-- message to learn the person, the topic and the consent coordinates. Message retention purges
-- the message, so a link in a mail older than the window answered 404 and the recipient could not
-- opt out. Changing the token cannot help mail that was already sent.
--
-- The retention purge now writes one row here for each `sent` message it deletes, in the same
-- statement (`messages::retention`), and `find_tracked_message` falls back to it. A row holds what
-- the unsubscribe and preference routes read and nothing else: no address, no subject, no link.
-- Click links stay 404 after the purge: `message_links` holds URLs that retention is meant to
-- delete, so it is not kept.
--
-- A tombstone is never aged out: it is removed with its person (the erasure's DELETIONS, and the
-- cascade of the key), and the merge rewrite re-points it. About 150 bytes a purged message.
-- Messages purged before this release cannot be recovered; their links stay 404.
--
-- Plain CREATE of a new table: no lock on an existing one.
CREATE TABLE message_tombstones (
    message_id    uuid PRIMARY KEY,
    workspace_id  uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    person_id     uuid NOT NULL REFERENCES people (id) ON DELETE CASCADE,
    enrollment_id uuid NOT NULL,
    template_id   uuid NOT NULL,
    topic_id      uuid,
    -- The consent coordinates besides the topic: dimension key to value key.
    dimensions    jsonb NOT NULL DEFAULT '{}'::jsonb,
    status        text NOT NULL,
    completed_at  timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now()
);

-- Erasure and the merge rewrite find a person's rows here.
CREATE INDEX message_tombstones_person_idx ON message_tombstones (workspace_id, person_id);
