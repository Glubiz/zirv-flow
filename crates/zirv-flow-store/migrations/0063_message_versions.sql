-- The versions a message was rendered with (#307, architecture 11.4 and 11.7). A template
-- version pins its own content, but the theme and the snippets a block email uses are resolved
-- at send time as their current published versions, so the message log records which ones this
-- message used. A test send writes no `messages` row, so `test_sends` records the same.
--
-- None of these columns holds a person, an address or a value: a theme id, a version number and
-- a map from snippet id to version number. `messages` stays in the erasure scan and the merge
-- exactly as before, and `test_sends` keeps its recipient hash handling.
--
-- All three are null for a message that was not rendered (skipped, or failed before the render)
-- and for every message sent before this migration. `snippet_versions` is an object, empty when
-- the render used no snippet; `theme_id` is null when no theme was in effect.
ALTER TABLE messages
    ADD COLUMN theme_id uuid,
    ADD COLUMN theme_version integer CHECK (theme_version > 0),
    ADD COLUMN snippet_versions jsonb CHECK (jsonb_typeof(snippet_versions) = 'object');

ALTER TABLE test_sends
    ADD COLUMN theme_id uuid,
    ADD COLUMN theme_version integer CHECK (theme_version > 0),
    ADD COLUMN snippet_versions jsonb CHECK (jsonb_typeof(snippet_versions) = 'object');
