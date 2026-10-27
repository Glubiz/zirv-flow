-- The leader samples the live enrollments every few seconds for the `zirv_flow_enrollments` and
-- `zirv_flow_timer_lateness_seconds` gauges (architecture 17): how many are `active` and
-- `waiting`, and how long the oldest waiting timer has been past its `wake_at`. No existing index
-- leads with the state alone, so both reads would scan the table.
--
-- The index holds only the live rows, so it stays small next to the ended ones. The counts are
-- index-only scans of it, and the oldest overdue timer is the first entry of the `waiting`
-- range, a lookup that does not grow with the table.
--
-- This takes a SHARE lock on `enrollments` until it is built. A populated deployment builds it
-- first with CREATE INDEX CONCURRENTLY under the same name and definition.
CREATE INDEX enrollments_live_idx ON enrollments (state, wake_at)
    WHERE state IN ('active', 'waiting');
