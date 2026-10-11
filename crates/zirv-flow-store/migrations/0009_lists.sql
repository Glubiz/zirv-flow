-- Static lists (architecture sections 6 and 7.3): named sets of people, the memberships and
-- the history of every membership change. Every table is workspace-scoped and every index
-- leads with workspace_id, except the ones keyed by a list id, which is itself unique.
-- Person-scoped columns are named person_id so the erasure scan finds them.

CREATE TABLE lists (
    id           uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    name         text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 128),
    description  text NOT NULL DEFAULT '',
    -- Kept in the transaction of every membership change, under a lock on the list row.
    member_count bigint NOT NULL DEFAULT 0 CHECK (member_count >= 0),
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    created_by   uuid REFERENCES users (id)
);

CREATE UNIQUE INDEX lists_name_idx ON lists (workspace_id, lower(name));

CREATE TABLE list_members (
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    list_id      uuid NOT NULL REFERENCES lists (id) ON DELETE CASCADE,
    person_id    uuid NOT NULL REFERENCES people (id),
    added_at     timestamptz NOT NULL DEFAULT now(),
    source       text NOT NULL CHECK (source IN ('ui', 'api', 'import', 'merge', 'flow')),
    PRIMARY KEY (list_id, person_id)
);

CREATE INDEX list_members_person_idx ON list_members (workspace_id, person_id);
-- Newest first, for the member pages.
CREATE INDEX list_members_page_idx ON list_members (list_id, added_at DESC, person_id DESC);

CREATE TABLE list_member_changes (
    id            uuid PRIMARY KEY,
    workspace_id  uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    list_id       uuid NOT NULL REFERENCES lists (id) ON DELETE CASCADE,
    person_id     uuid NOT NULL REFERENCES people (id),
    action        text NOT NULL CHECK (action IN ('added', 'removed')),
    source        text NOT NULL CHECK (source IN ('ui', 'api', 'import', 'merge', 'flow')),
    occurred_at   timestamptz NOT NULL DEFAULT now(),
    actor_user_id uuid REFERENCES users (id),
    trace_id      uuid NOT NULL
);

CREATE INDEX list_member_changes_person_idx
    ON list_member_changes (workspace_id, person_id, occurred_at DESC, id DESC);
CREATE INDEX list_member_changes_list_idx ON list_member_changes (list_id);
