-- Frequency caps and quiet hours: gates 5 and 6 of the send pipeline (architecture 10.1).
--
-- Neither table is person-scoped: they are the workspace's settings, so the erasure scan has
-- nothing to look for in them. The cap reads `messages`, which is.

-- A cap says at most `message_limit` messages on `channel` to one person in any window of
-- `window_hours`. A workspace may have several; a message must pass all of them. `mode` is what
-- happens to a message that would exceed the cap: it is skipped, or sent when a slot frees up.
CREATE TABLE frequency_caps (
    id            uuid PRIMARY KEY,
    workspace_id  uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    channel       text NOT NULL CHECK (channel IN ('email')),
    window_hours  integer NOT NULL CHECK (window_hours BETWEEN 1 AND 744),
    message_limit integer NOT NULL CHECK (message_limit >= 1),
    mode          text NOT NULL CHECK (mode IN ('skip', 'defer')),
    created_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX frequency_caps_workspace_idx ON frequency_caps (workspace_id, channel);

-- The quiet hours of a workspace: none or one row. A period starts at `start_time` local on one
-- of `days` and ends at `end_time`; an end at or before the start is on the next day, and a
-- period that wraps midnight belongs to the day it starts on.
CREATE TABLE quiet_hours (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    start_time   time NOT NULL,
    end_time     time NOT NULL,
    days         text[] NOT NULL,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    CHECK (start_time <> end_time),
    CHECK (cardinality(days) > 0 AND days <@ ARRAY['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun']::text[])
);

-- The slot a message holds against the person's frequency caps: when it was reserved. Null until
-- gate 5 reserves it, and again after a quiet-hours deferral releases it.
ALTER TABLE messages ADD COLUMN cap_slot_at timestamptz;
CREATE INDEX messages_cap_slot_idx ON messages (workspace_id, person_id, channel, cap_slot_at)
    WHERE cap_slot_at IS NOT NULL AND NOT test;
