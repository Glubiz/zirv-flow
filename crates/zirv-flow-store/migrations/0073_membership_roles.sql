-- Workspace roles become admin, publisher, editor and viewer (architecture 16.2, ruling 92d5483d).
-- A `member` could edit but not publish, so it becomes an `editor`: no one gains a publishing
-- right. The constraint is dropped, the rows are mapped and the wider constraint added in the
-- one transaction sqlx runs this file in, so no row is ever checked against the wrong set.
ALTER TABLE memberships DROP CONSTRAINT memberships_role_check;

UPDATE memberships SET role = 'editor' WHERE role = 'member';

ALTER TABLE memberships
    ADD CONSTRAINT memberships_role_check
    CHECK (role IN ('admin', 'publisher', 'editor', 'viewer'));
