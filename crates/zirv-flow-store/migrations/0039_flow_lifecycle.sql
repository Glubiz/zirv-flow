-- The flow lifecycle (architecture 9.3 and 9.4): a paused flow holds the steps that fall due, and
-- an enrollment keeps the input it stopped on.
--
-- `held` is an enrollment whose step fell due while its flow was paused. It keeps its node, its
-- timer, its waits and its `state_version`; `resume_input` is the `flow.step` input to run again
-- when the flow resumes (`start`, `wake` or `event`, in the payload's own form). A `failed`
-- enrollment keeps the input of the attempt that failed in the same column, so an operator retry
-- runs that step again from the state it started from; it is null for a failure the engine itself
-- reported, which the engine's own resume handles.
ALTER TABLE enrollments DROP CONSTRAINT enrollments_state_check;
ALTER TABLE enrollments ADD CONSTRAINT enrollments_state_check
    CHECK (state IN ('active', 'waiting', 'held', 'completed', 'exited', 'failed'));
ALTER TABLE enrollments ADD COLUMN resume_input jsonb;
ALTER TABLE enrollments ADD CONSTRAINT enrollments_held_input_check
    CHECK (state <> 'held' OR resume_input IS NOT NULL);

-- A held enrollment still holds the person, as a failed one does.
DROP INDEX enrollments_in_flight_idx;
CREATE UNIQUE INDEX enrollments_in_flight_idx ON enrollments (flow_id, person_id)
    WHERE state IN ('active', 'waiting', 'held', 'failed');
