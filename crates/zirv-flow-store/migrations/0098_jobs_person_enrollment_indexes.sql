-- Erasure and the orphan purge found a person's jobs with predicates on `payload ->> 'person_id'`,
-- `'enrollment_id'`, `'merged_person_id'` and on kind `flow.enroll`, none of which an index
-- served, so `purge_jobs`, `strip_enrolls` and the jobs probe of `erasure_calls::handled` each
-- seq-scanned the whole table (1.5M jobs: 51k to 118k buffers and 145 to 510 ms per statement,
-- about six statements per `delete_person`).
--
--  * `jobs_person_idx`: the dead webhook.call, delivery.send, delivery.sync and
--    template.test_send jobs of a person.
--  * `jobs_enrollment_idx`: the dead flow.step jobs, and the delivery.sync jobs, of an enrollment.
--  * `jobs_merged_person_idx`: pending person.merge.rewrite jobs by the merged person.
--  * `jobs_enroll_kind_idx`: the flow.enroll jobs of a workspace, which `strip_enrolls` rewrites.
--
-- The first two are partial on `state = 'dead'`, so no ready or running row ever enters them and
-- the enqueue and claim of the queue pay nothing for them (a first version indexed every state and
-- doubled bulk enqueue and claim on 1.5M jobs). Dead rows are kept for inspection and carry a
-- person's id, so the purge must reach them. The ready and running rows are reached through the
-- queue's own indexes (`jobs_ready_kind_idx`, `jobs_workspace_running_idx`), by kind, which costs
-- the live backlog of those kinds and not the table. The other two indexes cover low-volume kinds.
--
-- UPGRADE COST: `jobs` is a plain table, but sqlx runs a migration file in one transaction, so
-- the indexes cannot be built CONCURRENTLY. Each build takes SHARE on `jobs` and holds it to the
-- commit: every enqueue, claim, heartbeat and completion waits until all four are built, and the
-- wait grows with the stored jobs. Plan the upgrade for a quiet window.
CREATE INDEX jobs_person_idx
    ON jobs (workspace_id, (payload ->> 'person_id'))
    WHERE state = 'dead' AND payload ->> 'person_id' IS NOT NULL;
CREATE INDEX jobs_enrollment_idx
    ON jobs (workspace_id, (payload ->> 'enrollment_id'))
    WHERE state = 'dead' AND payload ->> 'enrollment_id' IS NOT NULL;
CREATE INDEX jobs_merged_person_idx
    ON jobs ((payload ->> 'merged_person_id'))
    WHERE kind = 'person.merge.rewrite';
CREATE INDEX jobs_enroll_kind_idx
    ON jobs (workspace_id, id)
    WHERE kind = 'flow.enroll';
