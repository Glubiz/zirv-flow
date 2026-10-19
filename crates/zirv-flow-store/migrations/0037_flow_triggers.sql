-- Flow triggers (architecture 9.1 and 9.4). A schedule trigger's cron and zone are mirrored to a
-- generic `schedules` row the scheduler leader fires, as a segment's cadence and a source's cron
-- are. The row fires a `flow.enroll` job; deleting the flow deletes it.
ALTER TABLE schedules
    ADD COLUMN flow_id uuid REFERENCES flows (id) ON DELETE CASCADE;
CREATE UNIQUE INDEX schedules_flow_idx ON schedules (flow_id) WHERE flow_id IS NOT NULL;
