-- Automatic UTM parameters on email links (#240). The workspace setting is one jsonb document
-- (enabled, four patterns, the link domains); the default is disabled with the default patterns.
-- A template can opt out: `inherit` follows the workspace, `off` never tags.
ALTER TABLE workspaces
    ADD COLUMN utm jsonb NOT NULL DEFAULT '{"enabled": false, "source": "{workspace}", "medium": "email", "campaign": "{flow}", "content": "{template}", "domains": []}'::jsonb;

ALTER TABLE templates
    ADD COLUMN utm text NOT NULL DEFAULT 'inherit' CHECK (utm IN ('inherit', 'off'));
