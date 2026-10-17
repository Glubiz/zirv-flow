-- Hand-written counts over the seeded rows, independent of the segment compiler, for the
-- known definitions of defs/*.json: measure.sh compares them with the counts the API's preview
-- returns. psql variables: ws, hi (the preview's compiled_at, which fixes the 90 day window).
-- One JSON object on standard output.
SELECT :'hi'::timestamptz - interval '2160 hours' AS lo \gset
SELECT json_build_object(
    'trait_only', (SELECT count(*) FROM people
        WHERE workspace_id = :'ws' AND merged_into IS NULL
          AND traits ->> 'plan' = 'pro'
          AND (traits ->> 'age')::numeric BETWEEN 25 AND 45
          AND traits ->> 'marketing_opt_in' = 'true'),
    'agg90', (SELECT count(*) FROM
        (SELECT person_id FROM events
          WHERE workspace_id = :'ws' AND name = 'bench_order_placed'
            AND occurred_at > :'lo' AND occurred_at <= :'hi'
          GROUP BY person_id HAVING count(*) >= 2) o
        JOIN people p ON p.id = o.person_id
        WHERE p.workspace_id = :'ws' AND p.merged_into IS NULL AND p.traits ->> 'plan' = 'pro'),
    'sum_category90', (SELECT count(*) FROM
        (SELECT person_id FROM events
          WHERE workspace_id = :'ws' AND name = 'bench_order_placed'
            AND occurred_at > :'lo' AND occurred_at <= :'hi'
            AND properties ->> 'category' = 'books'
          GROUP BY person_id HAVING sum((properties ->> 'amount')::numeric) > 150.005) o
        JOIN people p ON p.id = o.person_id
        WHERE p.workspace_id = :'ws' AND p.merged_into IS NULL));
