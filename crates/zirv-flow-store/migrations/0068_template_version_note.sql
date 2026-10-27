-- A note on a published template version (architecture 11.7, #334): the marketer's one line for the
-- version history, written on the Review and publish page. Template text, like the content: it holds
-- no person data. The version row is immutable, so the note is written with the version.

ALTER TABLE template_versions
    ADD COLUMN note text,
    ADD CONSTRAINT template_versions_note_length CHECK (char_length(note) <= 500);
