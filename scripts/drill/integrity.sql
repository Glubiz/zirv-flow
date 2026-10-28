\set ON_ERROR_STOP on
-- Called with -v schema=public -v verify=true (false takes only the count snapshot).
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
        EXECUTE format('SELECT count(*) FROM %s', tab.oid::regclass) INTO n;
        INSERT INTO drill_counts VALUES (tab.relname, n);
    END LOOP;
END $$;

\if :verify
-- VALIDATE alone does not scan constraints already marked valid. Check the stored rows too,
-- so a broken FK inserted with triggers disabled cannot pass this drill.
DO $$
DECLARE
    c record;
    predicate text;
    all_present text;
    any_present text;
    invalid boolean;
BEGIN
    FOR c IN
        SELECT co.* FROM pg_constraint co JOIN pg_namespace ns ON ns.oid = co.connamespace
        WHERE ns.nspname = current_setting('drill.schema') AND co.contype IN ('c', 'f')
        ORDER BY co.oid
    LOOP
        IF NOT c.convalidated THEN
            EXECUTE format('ALTER TABLE %s VALIDATE CONSTRAINT %I', c.conrelid::regclass, c.conname);
        END IF;
        IF c.contype = 'c' THEN
            EXECUTE format('SELECT EXISTS (SELECT 1 FROM %s WHERE NOT (%s))',
                c.conrelid::regclass, pg_get_expr(c.conbin, c.conrelid)) INTO invalid;
        ELSE
            SELECT string_agg(format('p.%I OPERATOR(%I.%s) child.%I', pa.attname, ns.nspname,
                                     op.oprname, ca.attname), ' AND ' ORDER BY k.n),
                   string_agg(format('child.%I IS NOT NULL', ca.attname), ' AND ' ORDER BY k.n),
                   string_agg(format('child.%I IS NOT NULL', ca.attname), ' OR ' ORDER BY k.n)
            INTO predicate, all_present, any_present
            FROM unnest(c.conkey, c.confkey, c.conpfeqop) WITH ORDINALITY AS k(child, parent, eq, n)
            JOIN pg_attribute ca ON ca.attrelid = c.conrelid AND ca.attnum = k.child
            JOIN pg_attribute pa ON pa.attrelid = c.confrelid AND pa.attnum = k.parent
            JOIN pg_operator op ON op.oid = k.eq
            JOIN pg_namespace ns ON ns.oid = op.oprnamespace;
            EXECUTE format(
                'SELECT EXISTS (SELECT 1 FROM %s child WHERE ((%s) AND NOT EXISTS '
                '(SELECT 1 FROM %s p WHERE %s)) OR (%L = ''f'' AND (%s) AND NOT (%s)))',
                c.conrelid::regclass, all_present, c.confrelid::regclass, predicate,
                c.confmatchtype, any_present, all_present) INTO invalid;
        END IF;
        IF invalid THEN
            RAISE EXCEPTION 'integrity check: constraint violation';
        END IF;
    END LOOP;
    IF EXISTS (
        SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid = i.indrelid
        JOIN pg_namespace ns ON ns.oid = c.relnamespace
        WHERE ns.nspname = current_setting('drill.schema') AND NOT i.indisvalid
    ) THEN
        RAISE EXCEPTION 'integrity check: invalid index';
    END IF;
END $$;
\endif

SELECT coalesce(jsonb_object_agg(table_name, row_count ORDER BY table_name), '{}'::jsonb)
FROM drill_counts;
ROLLBACK;
