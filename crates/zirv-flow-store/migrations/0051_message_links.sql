-- The links of a sent message that click tracking rewrote (architecture 10.2). The tracked URL of
-- a message holds a token with the message id and a link index only; the redirect looks the
-- destination up here, so a token can never lead anywhere the message did not link to.
--
-- `message_links` is message-scoped, not person-scoped: it goes with its message, which erasure
-- deletes with the person (`messages` cascades here, and the erasure step also names the table).
-- A URL is the one in the rendered message and may carry what the template put in it, so it is
-- never logged or put in an event.

CREATE TABLE message_links (
    workspace_id uuid    NOT NULL,
    message_id   uuid    NOT NULL,
    -- Counts the rewritten links of the message from 0, in document order; a token carries it as
    -- an unsigned 16-bit number.
    link_index   integer NOT NULL CHECK (link_index BETWEEN 0 AND 65535),
    url          text    NOT NULL,
    PRIMARY KEY (message_id, link_index),
    FOREIGN KEY (workspace_id, message_id) REFERENCES messages (workspace_id, id) ON DELETE CASCADE
);
