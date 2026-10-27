-- The last validation of a template draft (architecture 11.7, #332): `{"blocking": n, "warnings": n}`
-- counts of the issues the publish checks find. A draft save stores it after the save commits and
-- clears it in the same save, so null means the draft has not been checked yet (or the check
-- could not run). The counts hold no issue text and no document content.

ALTER TABLE template_drafts ADD COLUMN checks jsonb;
