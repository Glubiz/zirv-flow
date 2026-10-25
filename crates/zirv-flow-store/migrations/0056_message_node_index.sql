-- The delivery counts of a flow version (the node-count read) group the version's message rows by
-- node. No earlier index leads with the flow, so that read would scan the workspace's log.
CREATE INDEX messages_flow_node_idx ON messages (workspace_id, flow_id, flow_version, node_id)
    WHERE flow_id IS NOT NULL;
