-- People search and the person timeline (architecture section 12.3).

-- Every decision about a person carries subject_ids.person, the person whose timeline it
-- belongs on. The timeline and erasure read decisions by that key. Created on the
-- partitioned parent, so every partition gets it.
CREATE INDEX decision_records_person_idx
    ON decision_records (workspace_id, (subject_ids ->> 'person'), recorded_at DESC)
    WHERE subject_ids ? 'person';

-- Trait search by equality uses containment. Range, prefix and existence searches scan under
-- a statement timeout until the indexing question (OQ-1) is decided.
CREATE INDEX people_traits_idx ON people USING gin (traits jsonb_path_ops);
