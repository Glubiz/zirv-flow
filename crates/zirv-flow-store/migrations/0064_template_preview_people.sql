-- Saved preview people (architecture 11.7 and 16.3, #308): the people an admin keeps on a
-- template to preview it as.
--
-- The table is person-scoped, so the erasure scan finds it by the `person_id` column and the
-- erasure deletes the rows in its transaction. A merge moves a loser's rows to the survivor and
-- drops the ones the survivor already holds on that template. The person key cascades, so a row
-- can never outlive its person. At most 20 rows per template, held by the store under the
-- template's row lock. `added_by` is the user who saved the person; null for an API key.

CREATE TABLE template_preview_people (
    template_id  uuid NOT NULL,
    workspace_id uuid NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    person_id    uuid NOT NULL REFERENCES people (id) ON DELETE CASCADE,
    added_by     uuid REFERENCES users (id) ON DELETE SET NULL,
    created_at   timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (template_id, person_id),
    FOREIGN KEY (workspace_id, template_id) REFERENCES templates (workspace_id, id) ON DELETE CASCADE
);

CREATE INDEX template_preview_people_person_idx
    ON template_preview_people (workspace_id, person_id);
