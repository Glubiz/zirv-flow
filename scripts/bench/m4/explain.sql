-- The statement a segment preview runs for one of the M4 definitions, written out by hand from
-- the compiler's lowering (zirv-flow-segments/src/lower.rs, compile.rs `query`), so it can be
-- run under EXPLAIN (ANALYZE, BUFFERS) or as a plain count, and so the slot indexes show in the
-- plan. The compiler lives in the app and the store cannot print the SQL it runs, so this is a
-- mirror: measure.sh checks it returns the same count as the API's preview before it trusts the
-- plan. Run through psql with these variables:
--   ws        the workspace id
--   variant   trait_jsonb | trait_slots | agg90 | sum90
--   mode      explain | count
--   hi        (optional) the window end, an ISO instant; default now(). The preview's
--             `compiled_at` makes the mirror see the same 90 days the app did.
--   slot_plan, slot_age, slot_opt   (trait_slots; optional for agg90) the ready slot columns of
--             the traits plan, age and marketing_opt_in, read from trait_definitions
--
-- The definitions are defs/trait_only.json, defs/agg90.json and defs/sum_category90.json. A string
-- or boolean `eq` over jsonb is a containment (`p.traits @> {...}`, served by the GIN index); a
-- number is the guarded cast of number_sql; a slot leaf is `col = value AND col IS NOT NULL`.
\if :{?hi}
\else
  SELECT now() AS hi \gset
\endif
SELECT :'hi'::timestamptz - interval '2160 hours' AS lo \gset
SELECT :'variant' = 'trait_jsonb' AS v_tj, :'variant' = 'trait_slots' AS v_ts,
       :'variant' = 'agg90' AS v_agg, :'variant' = 'sum90' AS v_sum,
       :'mode' = 'explain' AS do_explain \gset

\if :v_tj
  SELECT format($f$p.traits @> %L::jsonb
      AND COALESCE((CASE WHEN jsonb_typeof((p.traits -> 'age')) = 'number' THEN CASE WHEN abs(((p.traits -> 'age'))::numeric) <= 1e300 THEN ((p.traits -> 'age'))::float8 END END) BETWEEN 25::float8 AND 45::float8, false)
      AND p.traits @> %L::jsonb$f$,
      '{"plan":"pro"}', '{"marketing_opt_in":true}') AS pred \gset
\elif :v_ts
  SELECT format($f$(p.%1$I = 'pro' AND p.%1$I IS NOT NULL)
      AND (p.%2$I BETWEEN 25::float8 AND 45::float8 AND p.%2$I IS NOT NULL)
      AND (p.%3$I IS TRUE AND p.%3$I IS NOT NULL)$f$, :'slot_plan', :'slot_age', :'slot_opt') AS pred \gset
\elif :v_agg
  -- The trait leaf reads its slot once the trait is ready, else jsonb.
  \if :{?slot_plan}
    SELECT format($f$(p.%1$I = 'pro' AND p.%1$I IS NOT NULL)$f$, :'slot_plan') AS plan_leaf \gset
  \else
    SELECT format($f$p.traits @> %L::jsonb$f$, '{"plan":"pro"}') AS plan_leaf \gset
  \endif
  SELECT format($f$%s
      AND COALESCE(((SELECT count(*)::float8 FROM events e WHERE e.workspace_id = p.workspace_id
            AND e.person_id = p.id AND e.name = 'bench_order_placed'
            AND e.occurred_at > %L::timestamptz AND e.occurred_at <= %L::timestamptz) >= 2::float8), false)$f$,
      :'plan_leaf', :'lo', :'hi') AS pred \gset
\elif :v_sum
  SELECT format($f$COALESCE(((SELECT sum(CASE WHEN jsonb_typeof((e.properties -> 'amount')) = 'number' THEN CASE WHEN abs(((e.properties -> 'amount'))::numeric) <= 1e300 THEN ((e.properties -> 'amount'))::float8 END END)
            FROM events e WHERE e.workspace_id = p.workspace_id AND e.person_id = p.id
            AND e.name = 'bench_order_placed'
            AND e.occurred_at > %L::timestamptz AND e.occurred_at <= %L::timestamptz
            AND e.properties @> %L::jsonb) > 150.005::float8), false)$f$,
      :'lo', :'hi', '{"category":"books"}') AS pred \gset
\else
  \echo unknown variant :variant
  \quit 3
\endif

\if :do_explain
  EXPLAIN (ANALYZE, BUFFERS, SETTINGS)
  SELECT count(*) FROM people p WHERE p.workspace_id = :'ws' AND p.merged_into IS NULL AND (:pred);
\else
  SELECT count(*) FROM people p WHERE p.workspace_id = :'ws' AND p.merged_into IS NULL AND (:pred);
\endif
