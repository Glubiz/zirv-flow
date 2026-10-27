-- One row naming the life of the unlogged debugger feed. UNLOGGED, like the feed: a crash
-- empties both, and the next reader fills this one again with a new value. The debugger puts
-- it in every event id, so a client that resumes from before a crash is told the stream was
-- reset instead of silently skipping rows of the restarted sequence.
CREATE UNLOGGED TABLE debugger_epoch (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    epoch     text NOT NULL
);
