-- The flow enrollment inspector (architecture 12.3 and 12.4): the indexes its reads need, and
-- nothing else. Each is checked with EXPLAIN by `flows::inspector::tests`.

-- The enrollment list of a flow, newest first, paged by the keyset (created_at, id). The flow
-- comes first because every read names one.
CREATE INDEX enrollments_flow_list_idx ON enrollments (flow_id, created_at DESC, id DESC);

-- The same list filtered by state: the equality column sits before the order columns, so the
-- filtered page is an index range scan and not a sort.
CREATE INDEX enrollments_flow_state_list_idx
    ON enrollments (flow_id, state, created_at DESC, id DESC);

-- The same list filtered by version, and the per-node counts of one version, which join the
-- enrollments of (flow, version) to their step executions through `id`; `id` is in the key, so
-- that join reads the index alone. The existing `enrollments_flow_state_idx` serves the
-- "waiting now" count (flow, state, version).
CREATE INDEX enrollments_flow_version_list_idx
    ON enrollments (flow_id, flow_version, created_at DESC, id DESC);

-- One enrollment's decisions, in order. Flow decisions carry `subject_ids.enrollment`. Like the
-- person index of 0012 it is created on the partitioned parent, so every partition gets it, and
-- it covers only the rows that have the key.
CREATE INDEX decision_records_enrollment_idx
    ON decision_records (workspace_id, (subject_ids ->> 'enrollment'), recorded_at)
    WHERE subject_ids ? 'enrollment';
