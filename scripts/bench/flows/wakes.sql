-- One sample of the pending wake jobs (unique key `wake:<enrollment id>:<state version>`, run_at =
-- the wake time of the wait). A 30 s wait is in the queue for 30 s, so a sample every 2 s sees it.
INSERT INTO flowbench.wakes (enrollment_id, wake_at)
SELECT split_part(unique_key, ':', 2)::uuid, run_at
FROM jobs
WHERE kind = 'flow.step' AND state = 'ready' AND run_at > now() AND unique_key LIKE 'wake:%'
ON CONFLICT DO NOTHING;
