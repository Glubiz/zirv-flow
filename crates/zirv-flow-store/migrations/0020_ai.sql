-- The AI provider layer (architecture sections 13.1 and 13.4): a workspace's assistant
-- settings, a content-free log of every model call, and opt-in transcripts that expire.

-- One row per workspace that has saved assistant settings; a workspace without a row has the
-- assistant off. The provider key itself is an `encrypted_secrets` row with purpose
-- 'ai_provider_key'; this table holds only its id.
CREATE TABLE ai_settings (
    workspace_id          uuid PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    enabled               boolean NOT NULL DEFAULT false,
    provider_id           text,
    model                 text,
    -- Micro-units of the deployment currency per calendar month (UTC). 0 allows no calls.
    monthly_budget_micros bigint NOT NULL DEFAULT 0 CHECK (monthly_budget_micros >= 0),
    -- Whether requests may carry values of people (identifiers and properties).
    pii_opt_in            boolean NOT NULL DEFAULT false,
    -- NULL stores no transcripts.
    transcript_days       integer CHECK (transcript_days BETWEEN 1 AND 30),
    provider_key_secret_id uuid REFERENCES encrypted_secrets (id) ON DELETE SET NULL,
    -- The last capability probe: {"ok": bool, "missing": [{"capability", "reason"}]}.
    last_probe            jsonb,
    last_probe_at         timestamptz,
    updated_at            timestamptz NOT NULL DEFAULT now(),
    updated_by            uuid REFERENCES users (id) ON DELETE SET NULL
);

-- One row per dispatched model call, whatever the outcome. It holds counts and names only:
-- never a prompt, a response or an error body. A call refused by a gate is not dispatched and
-- leaves no row. The row is reserved 'pending' at its worst-case cost before the request, so the
-- rate limit and the budget hold under concurrent calls, and finished when the call ends.
CREATE TABLE ai_calls (
    id                    uuid PRIMARY KEY,
    workspace_id          uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    user_id               uuid REFERENCES users (id) ON DELETE SET NULL,
    feature               text NOT NULL CHECK (length(feature) BETWEEN 1 AND 64),
    provider_id           text NOT NULL,
    model                 text NOT NULL,
    input_tokens          integer NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens         integer NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    estimated_cost_micros bigint NOT NULL DEFAULT 0 CHECK (estimated_cost_micros >= 0),
    latency_ms            integer NOT NULL DEFAULT 0 CHECK (latency_ms >= 0),
    outcome               text NOT NULL CHECK (outcome IN
        ('pending', 'ok', 'http_error', 'timeout', 'egress_denied', 'transport', 'malformed',
         'capability')),
    draft_id              uuid,
    created_at            timestamptz NOT NULL DEFAULT now()
);

-- The monthly spend and the usage page read a workspace's recent calls.
CREATE INDEX ai_calls_workspace_created_idx ON ai_calls (workspace_id, created_at);
-- The per-user rate limit counts one user's calls in the last minute.
CREATE INDEX ai_calls_user_created_idx ON ai_calls (workspace_id, user_id, created_at);

-- The request and response parts of a call, kept only while the workspace's admin has opted in,
-- for 1 to 30 days. A retention job deletes them once expired; an erasure deletes the ones that
-- name the erased person.
CREATE TABLE ai_transcripts (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    call_id      uuid NOT NULL REFERENCES ai_calls (id) ON DELETE CASCADE,
    user_id      uuid REFERENCES users (id) ON DELETE SET NULL,
    content      jsonb NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL
);

CREATE INDEX ai_transcripts_expires_idx ON ai_transcripts (expires_at);
CREATE INDEX ai_transcripts_call_idx ON ai_transcripts (call_id);
CREATE INDEX ai_transcripts_workspace_idx ON ai_transcripts (workspace_id);

-- The hourly transcript sweep, as an instance-level schedule at minute 23, first at the next one.
INSERT INTO schedules (id, workspace_id, kind, cron, timezone, next_run_at, payload, enabled)
SELECT gen_random_uuid(), NULL, 'maintenance.ai_transcripts', '23 * * * *', 'UTC',
       CASE
           WHEN date_trunc('hour', now()) + interval '23 minutes' > now()
               THEN date_trunc('hour', now()) + interval '23 minutes'
           ELSE date_trunc('hour', now()) + interval '1 hour 23 minutes'
       END,
       '{}'::jsonb, true
WHERE NOT EXISTS (
    SELECT 1 FROM schedules WHERE kind = 'maintenance.ai_transcripts' AND workspace_id IS NULL
);
