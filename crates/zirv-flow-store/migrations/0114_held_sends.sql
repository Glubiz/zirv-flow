-- A paused flow holds the `delivery.send` jobs it meets instead of dropping them (#589). A send
-- that finds its flow paused ends its job and leaves its continuation here, one row per message,
-- written in the job's own transaction; the resume (and the archive) of the flow reads the rows
-- back in bounded batches, deleting them and enqueueing the jobs in one transaction, so a parked
-- send is either here or queued, never both and never neither.
--
-- `payload`, `max_attempts`, `unique_key` and the trace columns are what the job that carries the
-- send on would have been enqueued with. The message row exists (`pending`) before the row here
-- does, and the foreign key removes the parked send with the message when the person is erased.
-- The payload holds ids, never an address or a trait, like the job payload it stands for.
--
-- INDEX: the one index serves the keyset drain of a flow's parked sends in message id order. This
-- is a new table of low volume (only the sends that meet a paused flow, deleted by the resume),
-- not a hot table, so the write-cost rule for the hot tables does not apply to it.
--
-- UPGRADE COST: a new table and one index on it; the foreign keys take a brief lock on `messages`
-- and `workspaces`, which are not scanned. No existing row is rewritten.
CREATE TABLE held_sends (
    message_id   uuid        PRIMARY KEY REFERENCES messages (id) ON DELETE CASCADE,
    workspace_id uuid        NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    flow_id      uuid        NOT NULL,
    payload      jsonb       NOT NULL,
    max_attempts integer     NOT NULL CHECK (max_attempts >= 1),
    unique_key   text        NOT NULL,
    trace_id     uuid        NOT NULL,
    cause_kind   text        NOT NULL,
    cause_id     text        NOT NULL,
    held_at      timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX held_sends_flow_idx ON held_sends (workspace_id, flow_id, message_id);
