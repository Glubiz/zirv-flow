-- File sources (architecture sections 7.2 and 12.1): a sync run adds the people a mapped list
-- column names to the list, so a membership can come from a sync.
ALTER TABLE list_members DROP CONSTRAINT list_members_source_check;
ALTER TABLE list_members ADD CONSTRAINT list_members_source_check
    CHECK (source IN ('ui', 'api', 'import', 'merge', 'flow', 'sync'));
ALTER TABLE list_member_changes DROP CONSTRAINT list_member_changes_source_check;
ALTER TABLE list_member_changes ADD CONSTRAINT list_member_changes_source_check
    CHECK (source IN ('ui', 'api', 'import', 'merge', 'flow', 'sync'));
