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
-- An existing install's `summary_state` is already caught up, so the job would skip the backfill
-- and the new table would only ever hold the days from the upgrade on. The `UPDATE` at the end
-- clears every workspace's rollup marks: the next run takes the first-run path (recount today,
-- then backfill the last 30 days newest first, a few days per run). `summary_daily` is recounted
-- along with it, a one-time cost; its older rows stay until each day is recounted, so the Home
-- summary shows no gap.
--
-- UPGRADE COST: a new table, no lock on an existing one, and one pass over the small
-- `summary_state` table (a row per workspace). The job's per-day count reads
-- `messages_template_idx`, which exists.
CREATE TABLE template_use_daily (
    workspace_id uuid   NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    template_id  uuid   NOT NULL,
    day          date   NOT NULL,
    uses         bigint NOT NULL CHECK (uses > 0),
    PRIMARY KEY (workspace_id, template_id, day)
);

UPDATE summary_state SET counted_to = NULL, closed_through = NULL, backfilled_from = NULL;
