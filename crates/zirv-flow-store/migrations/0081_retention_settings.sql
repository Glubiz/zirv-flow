-- Workspace retention overrides and the last run of each retention window (architecture 16.3).
-- Neither table has a person column: they hold day counts and run counts only.

-- A workspace admin may shorten the message and decision-log windows. NULL means the deployment
-- value. The API refuses a value outside the deployment bounds or longer than the deployment
-- window; the check here only keeps a row positive.
CREATE TABLE workspace_retention (
    workspace_id  uuid PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    messages_days integer CHECK (messages_days > 0),
    decision_days integer CHECK (decision_days > 0),
    updated_at    timestamptz NOT NULL DEFAULT now(),
    updated_by    uuid REFERENCES users (id) ON DELETE SET NULL
);

-- The last run of each window, one row per window, written at the end of the job that enforces
-- it. Instance data: no workspace, no person.
CREATE TABLE retention_runs (
    "window" text PRIMARY KEY,
    ran_at   timestamptz NOT NULL,
    deleted  bigint NOT NULL CHECK (deleted >= 0)
);
