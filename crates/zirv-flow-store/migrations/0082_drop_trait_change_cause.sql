-- Ruling 67953328: `trait_change` leaves the cause kinds. v1 has no producer for it, so no row can
-- hold it; the migration asserts that before it narrows the four CHECKs (jobs, decision_records,
-- person_changes, enrollments). It returns when computed traits produce a cause of their own.
--
-- Locking: the four ALTER TABLEs take ACCESS EXCLUSIVE on jobs, enrollments, person_changes and
-- decision_records (every partition) and each ADD CONSTRAINT scans the whole table to validate
-- the new CHECK, as 0022 did. sqlx runs the migration in one transaction, so the locks last until
-- it commits. On a large install this runs during a stop of all roles.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM jobs WHERE cause_kind = 'trait_change')
        OR EXISTS (SELECT 1 FROM decision_records WHERE cause_kind = 'trait_change')
        OR EXISTS (SELECT 1 FROM person_changes WHERE cause_kind = 'trait_change')
        OR EXISTS (SELECT 1 FROM enrollments WHERE cause_kind = 'trait_change')
    THEN
        RAISE EXCEPTION 'a row holds the cause kind trait_change, which has no producer';
    END IF;
END
$$;

ALTER TABLE jobs DROP CONSTRAINT jobs_cause_kind_check;
ALTER TABLE jobs ADD CONSTRAINT jobs_cause_kind_check
    CHECK (cause_kind IN ('event', 'schedule', 'manual', 'api', 'definition_change', 'sync_run'));
ALTER TABLE decision_records DROP CONSTRAINT decision_records_cause_kind_check;
ALTER TABLE decision_records ADD CONSTRAINT decision_records_cause_kind_check
    CHECK (cause_kind IN ('event', 'schedule', 'manual', 'api', 'definition_change', 'sync_run'));
ALTER TABLE person_changes DROP CONSTRAINT person_changes_cause_kind_check;
ALTER TABLE person_changes ADD CONSTRAINT person_changes_cause_kind_check
    CHECK (cause_kind IN ('event', 'schedule', 'manual', 'api', 'definition_change', 'sync_run'));
ALTER TABLE enrollments DROP CONSTRAINT enrollments_cause_kind_check;
ALTER TABLE enrollments ADD CONSTRAINT enrollments_cause_kind_check
    CHECK (cause_kind IN ('event', 'schedule', 'manual', 'api', 'definition_change', 'sync_run'));
