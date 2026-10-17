-- KNOWN LIMITATION (supervisor ruling ed1f365c): this migration builds 30 partial indexes on `people`
-- in one transaction, which blocks writes to `people` for the duration of the build on a
-- populated install. Index creation must be revisited (for example CREATE INDEX CONCURRENTLY
-- outside the migration transaction) before upgrades of populated installations are supported.
--
-- Typed slot columns for hot traits (architecture sections 6 and 21.1, ruling a4e2afc1). `people`
-- gains 30 nullable columns: 8 string, 8 number, 6 timestamp, 4 date and 4 boolean slots. A trait
-- definition holds at most one slot of its type. A slot is only ever a copy of the value in
-- `people.traits`, written by the same statement as the `jsonb` value, so dropping the
-- assignment loses nothing. A date is stored at UTC midnight. `string_list` stays `jsonb`.
--
-- The lock order that keeps a backfill race-free (spec D3): every writer of `people.traits` takes
-- `pg_advisory_xact_lock_shared(7000, hashtext(workspace))` before it reads the assignments, and
-- every change of an assignment takes the exclusive form of the same lock.

ALTER TABLE people
    ADD COLUMN trait_s1 text,
    ADD COLUMN trait_s2 text,
    ADD COLUMN trait_s3 text,
    ADD COLUMN trait_s4 text,
    ADD COLUMN trait_s5 text,
    ADD COLUMN trait_s6 text,
    ADD COLUMN trait_s7 text,
    ADD COLUMN trait_s8 text,
    ADD COLUMN trait_n1 double precision,
    ADD COLUMN trait_n2 double precision,
    ADD COLUMN trait_n3 double precision,
    ADD COLUMN trait_n4 double precision,
    ADD COLUMN trait_n5 double precision,
    ADD COLUMN trait_n6 double precision,
    ADD COLUMN trait_n7 double precision,
    ADD COLUMN trait_n8 double precision,
    ADD COLUMN trait_t1 timestamptz,
    ADD COLUMN trait_t2 timestamptz,
    ADD COLUMN trait_t3 timestamptz,
    ADD COLUMN trait_t4 timestamptz,
    ADD COLUMN trait_t5 timestamptz,
    ADD COLUMN trait_t6 timestamptz,
    ADD COLUMN trait_d1 timestamptz,
    ADD COLUMN trait_d2 timestamptz,
    ADD COLUMN trait_d3 timestamptz,
    ADD COLUMN trait_d4 timestamptz,
    ADD COLUMN trait_b1 boolean,
    ADD COLUMN trait_b2 boolean,
    ADD COLUMN trait_b3 boolean,
    ADD COLUMN trait_b4 boolean;

CREATE INDEX people_trait_s1_idx ON people (workspace_id, trait_s1) WHERE trait_s1 IS NOT NULL;
CREATE INDEX people_trait_s2_idx ON people (workspace_id, trait_s2) WHERE trait_s2 IS NOT NULL;
CREATE INDEX people_trait_s3_idx ON people (workspace_id, trait_s3) WHERE trait_s3 IS NOT NULL;
CREATE INDEX people_trait_s4_idx ON people (workspace_id, trait_s4) WHERE trait_s4 IS NOT NULL;
CREATE INDEX people_trait_s5_idx ON people (workspace_id, trait_s5) WHERE trait_s5 IS NOT NULL;
CREATE INDEX people_trait_s6_idx ON people (workspace_id, trait_s6) WHERE trait_s6 IS NOT NULL;
CREATE INDEX people_trait_s7_idx ON people (workspace_id, trait_s7) WHERE trait_s7 IS NOT NULL;
CREATE INDEX people_trait_s8_idx ON people (workspace_id, trait_s8) WHERE trait_s8 IS NOT NULL;
CREATE INDEX people_trait_n1_idx ON people (workspace_id, trait_n1) WHERE trait_n1 IS NOT NULL;
CREATE INDEX people_trait_n2_idx ON people (workspace_id, trait_n2) WHERE trait_n2 IS NOT NULL;
CREATE INDEX people_trait_n3_idx ON people (workspace_id, trait_n3) WHERE trait_n3 IS NOT NULL;
CREATE INDEX people_trait_n4_idx ON people (workspace_id, trait_n4) WHERE trait_n4 IS NOT NULL;
CREATE INDEX people_trait_n5_idx ON people (workspace_id, trait_n5) WHERE trait_n5 IS NOT NULL;
CREATE INDEX people_trait_n6_idx ON people (workspace_id, trait_n6) WHERE trait_n6 IS NOT NULL;
CREATE INDEX people_trait_n7_idx ON people (workspace_id, trait_n7) WHERE trait_n7 IS NOT NULL;
CREATE INDEX people_trait_n8_idx ON people (workspace_id, trait_n8) WHERE trait_n8 IS NOT NULL;
CREATE INDEX people_trait_t1_idx ON people (workspace_id, trait_t1) WHERE trait_t1 IS NOT NULL;
CREATE INDEX people_trait_t2_idx ON people (workspace_id, trait_t2) WHERE trait_t2 IS NOT NULL;
CREATE INDEX people_trait_t3_idx ON people (workspace_id, trait_t3) WHERE trait_t3 IS NOT NULL;
CREATE INDEX people_trait_t4_idx ON people (workspace_id, trait_t4) WHERE trait_t4 IS NOT NULL;
CREATE INDEX people_trait_t5_idx ON people (workspace_id, trait_t5) WHERE trait_t5 IS NOT NULL;
CREATE INDEX people_trait_t6_idx ON people (workspace_id, trait_t6) WHERE trait_t6 IS NOT NULL;
CREATE INDEX people_trait_d1_idx ON people (workspace_id, trait_d1) WHERE trait_d1 IS NOT NULL;
CREATE INDEX people_trait_d2_idx ON people (workspace_id, trait_d2) WHERE trait_d2 IS NOT NULL;
CREATE INDEX people_trait_d3_idx ON people (workspace_id, trait_d3) WHERE trait_d3 IS NOT NULL;
CREATE INDEX people_trait_d4_idx ON people (workspace_id, trait_d4) WHERE trait_d4 IS NOT NULL;
CREATE INDEX people_trait_b1_idx ON people (workspace_id, trait_b1) WHERE trait_b1 IS NOT NULL;
CREATE INDEX people_trait_b2_idx ON people (workspace_id, trait_b2) WHERE trait_b2 IS NOT NULL;
CREATE INDEX people_trait_b3_idx ON people (workspace_id, trait_b3) WHERE trait_b3 IS NOT NULL;
CREATE INDEX people_trait_b4_idx ON people (workspace_id, trait_b4) WHERE trait_b4 IS NOT NULL;

ALTER TABLE trait_definitions
    -- The slot column the trait is promoted to, and where that is in its life: `backfilling`
    -- (written by every writer, not yet read), `ready` (read by the compiler) or `retiring`
    -- (neither read nor written, until the column is cleared and the slot freed).
    ADD COLUMN slot       text,
    ADD COLUMN slot_state text,
    -- Counts the assignments the trait has had; every slot job carries the one it was enqueued
    -- for, so a delayed job cannot complete a newer assignment of the same slot.
    ADD COLUMN slot_generation bigint NOT NULL DEFAULT 0,
    ADD CONSTRAINT trait_definitions_slot_matches_type CHECK (
        slot IS NULL
        OR (type = 'string'    AND slot ~ '^trait_s[1-8]$')
        OR (type = 'number'    AND slot ~ '^trait_n[1-8]$')
        OR (type = 'timestamp' AND slot ~ '^trait_t[1-6]$')
        OR (type = 'date'      AND slot ~ '^trait_d[1-4]$')
        OR (type = 'boolean'   AND slot ~ '^trait_b[1-4]$')
    ),
    ADD CONSTRAINT trait_definitions_slot_state CHECK (
        (slot IS NULL AND slot_state IS NULL)
        OR (slot IS NOT NULL AND slot_state IN ('backfilling', 'ready', 'retiring'))
    );

-- A slot is free when no definition in the workspace holds it.
CREATE UNIQUE INDEX trait_definitions_slot_idx ON trait_definitions (workspace_id, slot)
    WHERE slot IS NOT NULL;
