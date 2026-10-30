-- Load one chunk of a staged run into the product tables (seed.sh, step load). psql variables:
-- run, set ('spread' or 'burst'), lo, hi (the n range, inclusive). One transaction, so a failed
-- chunk leaves nothing behind. Each table gets rows of the template's exact shape:
-- jsonb_populate_record copies every column of the sample row and only the per-row values change.
-- M8 includes this inside the staging/checkpoint transaction.
\if :{?seed_outer_transaction}
\else
BEGIN;
\endif

INSERT INTO enrollments
SELECT (jsonb_populate_record(NULL::enrollments, t.row || jsonb_build_object(
    'id', r.enrollment_id, 'person_id', r.person_id, 'occurrence_id', r.occurrence_id,
    'trace_id', r.trace_id, 'wake_at', r.wake_at, 'created_at', r.created_at, 'updated_at', r.step_at,
    'context', (t.row -> 'context') || jsonb_build_object('occurrence', r.occurrence_id)))).*
FROM m5bench.rows r JOIN m5bench.tmpl t ON t.set = r.set AND t.kind = 'enrollment'
WHERE r.run = :run AND r.set = :'set' AND r.n BETWEEN :lo AND :hi;

INSERT INTO flow_entries
SELECT (jsonb_populate_record(NULL::flow_entries, t.row || jsonb_build_object(
    'person_id', r.person_id, 'last_entered_at', r.created_at))).*
FROM m5bench.rows r JOIN m5bench.tmpl t ON t.set = r.set AND t.kind = 'flow_entry'
WHERE r.run = :run AND r.set = :'set' AND r.n BETWEEN :lo AND :hi;

INSERT INTO step_executions
SELECT (jsonb_populate_record(NULL::step_executions, t.row || jsonb_build_object(
    'id', r.step_id, 'enrollment_id', r.enrollment_id, 'started_at', r.step_at))).*
FROM m5bench.rows r JOIN m5bench.tmpl t ON t.set = r.set AND t.kind = 'step'
WHERE r.run = :run AND r.set = :'set' AND r.n BETWEEN :lo AND :hi;

INSERT INTO jobs
SELECT (jsonb_populate_record(NULL::jobs, t.row || jsonb_build_object(
    'id', r.job_id, 'run_at', r.wake_at, 'created_at', r.step_at, 'trace_id', r.trace_id,
    'payload', (t.row -> 'payload') || jsonb_build_object('enrollment_id', r.enrollment_id),
    'unique_key', 'wake:' || r.enrollment_id || ':' || ((t.row -> 'payload') ->> 'state_version')))).*
FROM m5bench.rows r JOIN m5bench.tmpl t ON t.set = r.set AND t.kind = 'job'
WHERE r.run = :run AND r.set = :'set' AND r.n BETWEEN :lo AND :hi;

INSERT INTO decision_records
SELECT (jsonb_populate_record(NULL::decision_records, t.row || jsonb_build_object(
    'id', m5bench.uuid7(CASE WHEN t.row ->> 'action' = 'flow.enroll' THEN r.created_at ELSE r.step_at END),
    'trace_id', r.trace_id,
    'recorded_at', CASE WHEN t.row ->> 'action' = 'flow.enroll' THEN r.created_at ELSE r.step_at END,
    'job_id', CASE WHEN t.row -> 'job_id' = 'null'::jsonb THEN NULL ELSE r.start_job_id END,
    'subject_ids', (t.row -> 'subject_ids')
        || jsonb_build_object('person', r.person_id, 'enrollment', r.enrollment_id)
        || CASE WHEN (t.row -> 'subject_ids') ? 'step_execution'
                THEN jsonb_build_object('step_execution', r.step_id) ELSE '{}'::jsonb END))).*
FROM m5bench.rows r JOIN m5bench.tmpl t ON t.set = r.set AND t.kind = 'decision'
WHERE r.run = :run AND r.set = :'set' AND r.n BETWEEN :lo AND :hi;

\if :{?seed_outer_transaction}
\else
COMMIT;
\endif
