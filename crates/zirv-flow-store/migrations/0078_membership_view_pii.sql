-- The `view_pii` grant (architecture 16.2, ruling ca362e00). The owner and a workspace admin see
-- personal values by implication, so the flag only matters for publishers, editors and viewers.
-- Defaulting to false changes no one's access: today only owner, admin and `admin` keys see values.
ALTER TABLE memberships ADD COLUMN view_pii boolean NOT NULL DEFAULT false;
