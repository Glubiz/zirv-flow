-- The revision of a workspace's send settings (frequency caps and quiet hours). A replace names the
-- revision it read and is refused if the settings were replaced since, so a stale form cannot
-- overwrite newer settings. No row means revision 0.
CREATE TABLE send_settings (
    workspace_id uuid PRIMARY KEY REFERENCES workspaces (id) ON DELETE CASCADE,
    revision     integer NOT NULL,
    updated_at   timestamptz NOT NULL DEFAULT now()
);
