-- One open conflict per person pair, source record and shared identity: two identities shared
-- between the same two people are two conflicts, each movable through review.
-- Open duplicates under the wider key cannot exist, as the old key was narrower.
DROP INDEX identity_conflicts_open_idx;
CREATE UNIQUE INDEX identity_conflicts_open_idx ON identity_conflicts
    (workspace_id, person_id, other_person_id,
     (COALESCE(source_record_id, '00000000-0000-0000-0000-000000000000'::uuid)),
     (COALESCE(identity_id, '00000000-0000-0000-0000-000000000000'::uuid)))
    WHERE resolved_at IS NULL;
