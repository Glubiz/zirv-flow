-- Workspace roles become admin, publisher, editor and viewer (architecture 16.2, ruling 92d5483d).
-- A `member` could edit but not publish, so it becomes an `editor`: no one gains a publishing
-- right. The constraint is dropped, the rows are mapped and the wider constraint added in the
-- one transaction sqlx runs this file in, so no row is ever checked against the wrong set.
ALTER TABLE memberships DROP CONSTRAINT memberships_role_check;

UPDATE memberships SET role = 'editor' WHERE role = 'member';

ALTER TABLE memberships
    ADD CONSTRAINT memberships_role_check
    CHECK (role IN ('admin', 'publisher', 'editor', 'viewer'));

-- Publishing moves from the admin scope to the new `publish` scope, so a deployed admin key keeps
-- its rights by gaining it. Scopes never imply one another in code; `write` keys are left alone.
UPDATE api_keys SET scopes = array_append(scopes, 'publish')
WHERE 'admin' = ANY (scopes) AND NOT ('publish' = ANY (scopes));
