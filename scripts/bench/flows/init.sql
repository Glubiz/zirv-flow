-- Scratch table of the flow benchmark (scripts/bench/flows.sh): the wake time of each timer, copied
-- from the jobs table while it is still there. The runtime clears enrollments.wake_at on completion
-- and deletes the done flow.step job, so nothing else keeps the instant the wait was due.
-- It lives in its own schema, on the throwaway bench stack only.
CREATE SCHEMA IF NOT EXISTS flowbench;
CREATE TABLE IF NOT EXISTS flowbench.wakes (
    enrollment_id uuid PRIMARY KEY,
    wake_at       timestamptz NOT NULL,
    seen_at       timestamptz NOT NULL DEFAULT now()
);
