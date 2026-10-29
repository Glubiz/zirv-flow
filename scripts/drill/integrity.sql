\set ON_ERROR_STOP on
\set VERBOSITY terse
-- Called with -v schema=public -v verify=true (false takes only the count snapshot).
-- Migrations use the default public schema; tests pass their private scratch schema.
-- All changes, including VALIDATE CONSTRAINT, are rolled back. No persistent drill tables.
BEGIN;
SET LOCAL statement_timeout = '5min';
SET LOCAL lock_timeout = '5s';
SELECT set_config('drill.schema', :'schema', true) AS ignored \gset
CREATE TEMP TABLE drill_counts (table_name text PRIMARY KEY, row_count bigint NOT NULL);
DO $$
DECLARE
    tab record;
    n bigint;
BEGIN
    FOR tab IN
        SELECT c.oid, c.relname FROM pg_class c JOIN pg_namespace ns ON ns.oid = c.relnamespace
        WHERE ns.nspname = current_setting('drill.schema') AND c.relkind IN ('r', 'p')
        ORDER BY c.relname
    LOOP
        BEGIN
            EXECUTE format('SELECT count(*) FROM %s', tab.oid::regclass) INTO n;
            INSERT INTO drill_counts VALUES (tab.relname, n);
        EXCEPTION WHEN OTHERS THEN
            RAISE EXCEPTION 'integrity check=count table=%.% constraint=- sqlstate=%',
                current_setting('drill.schema'), tab.relname, SQLSTATE;
        END;
    END LOOP;
END $$;

\if :verify
-- VALIDATE alone does not scan constraints already marked valid. Check the stored rows too,
-- so a broken FK inserted with triggers disabled cannot pass this drill.
DO $$
DECLARE
    constraint_row record;
    index_row record;
    check_name text;
    predicate text;
    all_present text;
    any_present text;
    invalid boolean;
BEGIN
    FOR constraint_row IN
        SELECT co.*, rel.relname FROM pg_constraint co
        JOIN pg_class rel ON rel.oid = co.conrelid
        JOIN pg_namespace ns ON ns.oid = rel.relnamespace
        WHERE ns.nspname = current_setting('drill.schema') AND co.contype IN ('c', 'f')
          AND rel.relkind IN ('r', 'p')
        ORDER BY co.oid
    LOOP
        BEGIN
            check_name := 'validate_constraint';
            IF NOT constraint_row.convalidated THEN
                EXECUTE format('ALTER TABLE %s VALIDATE CONSTRAINT %I',
                    constraint_row.conrelid::regclass, constraint_row.conname);
            END IF;
            IF constraint_row.contype = 'c' THEN
                check_name := 'check_rows';
                EXECUTE format('SELECT EXISTS (SELECT 1 FROM %s WHERE NOT (%s))',
                    constraint_row.conrelid::regclass,
                    pg_get_expr(constraint_row.conbin, constraint_row.conrelid)) INTO invalid;
            ELSE
                check_name := 'foreign_key_rows';
                SELECT string_agg(format('p.%I OPERATOR(%I.%s) child.%I', pa.attname, ns.nspname,
                                         op.oprname, ca.attname), ' AND ' ORDER BY k.n),
                       string_agg(format('child.%I IS NOT NULL', ca.attname), ' AND ' ORDER BY k.n),
                       string_agg(format('child.%I IS NOT NULL', ca.attname), ' OR ' ORDER BY k.n)
                INTO predicate, all_present, any_present
                FROM unnest(constraint_row.conkey, constraint_row.confkey, constraint_row.conpfeqop)
                    WITH ORDINALITY AS k(child, parent, eq, n)
                JOIN pg_attribute ca ON ca.attrelid = constraint_row.conrelid AND ca.attnum = k.child
                JOIN pg_attribute pa ON pa.attrelid = constraint_row.confrelid AND pa.attnum = k.parent
                JOIN pg_operator op ON op.oid = k.eq
                JOIN pg_namespace ns ON ns.oid = op.oprnamespace;
                EXECUTE format(
                    'SELECT EXISTS (SELECT 1 FROM %s child WHERE ((%s) AND NOT EXISTS '
                    '(SELECT 1 FROM %s p WHERE %s)) OR (%L = ''f'' AND (%s) AND NOT (%s)))',
                    constraint_row.conrelid::regclass, all_present,
                    constraint_row.confrelid::regclass, predicate,
                    constraint_row.confmatchtype, any_present, all_present) INTO invalid;
            END IF;
            IF invalid THEN
                RAISE EXCEPTION 'stored rows violate constraint';
            END IF;
        EXCEPTION WHEN OTHERS THEN
            -- Never propagate PostgreSQL's DETAIL, which can include failing row values.
            RAISE EXCEPTION 'integrity check=% table=%.% constraint=% sqlstate=%',
                check_name, current_setting('drill.schema'), constraint_row.relname,
                constraint_row.conname, SQLSTATE;
        END;
    END LOOP;
    FOR index_row IN
        SELECT rel.relname, idx.relname AS index_name
        FROM pg_index i JOIN pg_class rel ON rel.oid = i.indrelid
        JOIN pg_class idx ON idx.oid = i.indexrelid
        JOIN pg_namespace ns ON ns.oid = rel.relnamespace
        WHERE ns.nspname = current_setting('drill.schema') AND NOT i.indisvalid
    LOOP
        RAISE EXCEPTION 'integrity check=valid_index table=%.% constraint=- index=%',
            current_setting('drill.schema'), index_row.relname, index_row.index_name;
    END LOOP;
END $$;
\endif

SELECT coalesce(jsonb_object_agg(table_name, row_count ORDER BY table_name), '{}'::jsonb)
FROM drill_counts;
ROLLBACK;
