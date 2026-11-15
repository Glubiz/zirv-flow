-- The delivery counts of a flow version (`version_node_counts`, the canvas overlay) group the
-- version's finished messages by node, status and reason. `messages_flow_node_idx` carries
-- neither status nor reason, so the read heap-scanned every message of the version (660k
-- messages: 166 ms and 17.4k buffers).
--
-- `messages_flow_node_status_idx` covers the grouped columns, so the read is an index-only scan
-- (it needs a vacuumed visibility map). It is partial on the terminal statuses of real flow
-- messages, the statement's own predicate, so a message enters it only when it ends, and the
-- pending and sending churn of a send does not touch it. `messages_flow_node_idx` stays.
--
-- The build takes a SHARE lock on `messages` until it is done, which blocks every send's insert
-- and settle. A populated deployment builds it first, by hand, with the statement below; the
-- migration then finds it and does nothing. A failed concurrent build leaves an invalid index of
-- its name: drop it and build it again before upgrading.
--
--     CREATE INDEX CONCURRENTLY messages_flow_node_status_idx
--         ON messages (workspace_id, flow_id, flow_version, node_id, status, reason)
--         WHERE flow_id IS NOT NULL AND NOT test AND status IN ('sent', 'skipped', 'failed');
CREATE INDEX IF NOT EXISTS messages_flow_node_status_idx
    ON messages (workspace_id, flow_id, flow_version, node_id, status, reason)
    WHERE flow_id IS NOT NULL AND NOT test AND status IN ('sent', 'skipped', 'failed');
