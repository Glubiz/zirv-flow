-- The template library's "most used" sort counted the non-test messages of each template over the
-- last 30 days with a correlated `count(*)` over `messages`: every matching index entry and, for
-- `NOT test`, every heap row of the window (667,000 messages of one template: 1,058 ms and
-- 673,000 buffers, linear in the 30-day volume). It now reads a daily count per template.
--
-- `template_use_daily` is filled by the existing `summary.rollup` job beside `summary_daily`
-- (0085): it adds the messages created since its last run to their UTC day and recounts a day
-- exactly once it has closed, and the first runs backfill the last 30 days, a few days per run.
-- The count is by the message's `created_at`, non-test, whatever its status, as the live count
-- was. There is no foreign key on `template_id`: the counts of a deleted template simply age out
-- with the 35-day purge the job already runs. It holds counts only: no person, no address.
--
-- Visible change: the "most used" window is the last 30 whole UTC days (today and the 29 before)
-- instead of the last 30 x 24 hours, and it lags the messages by the job's period (five minutes).
-- Until the backfill has walked the 30 days after an upgrade, older days read as 0 uses.
--
-- UPGRADE COST: a new table, no lock on an existing one. The job's per-day count reads
-- `messages_template_idx`, which exists.
CREATE TABLE template_use_daily (
    workspace_id uuid   NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    template_id  uuid   NOT NULL,
    day          date   NOT NULL,
    uses         bigint NOT NULL CHECK (uses > 0),
    PRIMARY KEY (workspace_id, template_id, day)
);
