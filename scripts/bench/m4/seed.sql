-- M4 benchmark data (issue #127, spec D3): :people people with the OQ-1 data shape and :events
-- events, written with generate_series into the real tables of a database the app has migrated
-- (people, identities, events and their indexes, with the GIN index on people.traits and the
-- slot columns left NULL), so the segment code reads exactly what the profile store and the
-- ingest path write. A fixed setseed makes the values repeatable; the ids are md5 of the
-- workspace and the row number, so they are repeatable too.
--
-- Expects psql variables (seed.sh sets them):
--   ws           the workspace id (setup.sh created it, with the trait definitions and the
--                five event schemas)
--   people       people to write
--   events       events to write
--   lg_pool      people given an `external:user_id` identity bench-u-0 .. bench-u-(lg_pool-1),
--                the ids loadgen sends (capped at people)
--   days         how far back events reach (365)
--   seed_at      the instant "now" for the data, as ISO text; events fall in
--                [seed_at - days, seed_at]
--   chunk        rows per statement, so a long seed commits as it goes
--
-- Stored shapes, each confirmed against the code and the migrations (see seed.sh's header):
--   people.traits         jsonb: strings as JSON strings, numbers as JSON numbers, booleans,
--                         timestamps as RFC 3339 text with `Z` (SecondsFormat::AutoSi), dates
--                         as YYYY-MM-DD, string lists as arrays (core people/traits.rs coerce)
--   people.trait_sources  jsonb: key -> the name of the source that supplied it (`api`)
--   events.properties     jsonb as sent: amount number, category string, quantity integer
--   events                partitioned by month on occurred_at; the monthly partitions of the
--                         migration cover the retention window, so no row lands in DEFAULT
--
-- Not seeded, because no segment preview, full refresh or incremental re-evaluation reads it:
-- source_records (the ledger), decision_records, person_changes, consent, lists, event_dedupe.
\set ON_ERROR_STOP on

SELECT setseed(0.42);

-- One call writes the people lo..hi. A SQL function so \gexec can run it chunk by chunk, each in
-- its own transaction, in this session (the seed is one stream of random()).
CREATE FUNCTION pg_temp.seed_people(ws uuid, lo bigint, hi bigint, seed_ts timestamptz) RETURNS void
LANGUAGE sql VOLATILE AS $$
    INSERT INTO people (id, workspace_id, traits, trait_sources, created_at, updated_at)
    SELECT md5(ws::text || '/p/' || g)::uuid,
           ws,
           jsonb_build_object(
               'country',          'C' || lpad((random() * 49)::int::text, 2, '0'),
               'plan',             (ARRAY['free', 'pro', 'team', 'enterprise'])[1 + (random() * 3)::int],
               'lifecycle',        (ARRAY['lead', 'trial', 'active', 'at_risk', 'churned', 'won_back'])[1 + (random() * 5)::int],
               'channel',          (ARRAY['web', 'ios', 'android', 'email', 'sms', 'store', 'partner', 'api'])[1 + (random() * 7)::int],
               'city',             'city-' || (random() * 999)::int,
               'region',           'R' || (random() * 11)::int,
               'age',              18 + (random() * 62)::int,
               'ltv',              round((random() * 1000)::numeric, 2),
               'seats',            (random() * 500)::int,
               'orders',           (random() * 100)::int,
               'score',            round(random()::numeric, 4),
               'marketing_opt_in', random() < 0.6,
               'email_verified',   random() < 0.9,
               'is_vip',           random() < 0.05,
               'trial',            random() < 0.2,
               'signup_at',        to_char(timestamp '2023-01-01' + random() * interval '1000 days', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
               'last_seen_at',     to_char(timestamp '2025-01-01' + random() * interval '270 days', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
               'birthday',         to_char(date '1950-01-01' + (random() * 20000)::int, 'YYYY-MM-DD'),
               'tags',             jsonb_build_array('t' || (random() * 20)::int, 't' || (random() * 20)::int),
               'interests',        jsonb_build_array('i' || (random() * 50)::int, 'i' || (random() * 50)::int, 'i' || (random() * 50)::int)
           ),
           (SELECT jsonb_object_agg(key, 'api') FROM trait_definitions WHERE workspace_id = ws AND NOT archived),
           seed_ts,
           seed_ts
    FROM generate_series(lo, hi) g
$$;

-- Events: a person drawn uniformly, a name by weight (40% page views, 20% items added, 25%
-- orders, 5% plan changes, 10% support), a time uniform over the window. Only bench_order_placed
-- carries properties (amount, category, quantity); the other names are bare. Event ids and
-- person ids are the same md5 forms as above.
CREATE FUNCTION pg_temp.seed_events(ws uuid, lo bigint, hi bigint, n_people bigint, n_days int, seed_ts timestamptz) RETURNS void
LANGUAGE sql VOLATILE AS $$
    INSERT INTO events (id, workspace_id, person_id, name, schema_version, occurred_at, received_at,
                        properties, source, message_id, trace_id, validation_state,
                        validation_reasons, flags)
    SELECT md5(ws::text || '/e/' || g)::uuid,
           ws,
           md5(ws::text || '/p/' || (1 + floor(random() * n_people)::bigint))::uuid,
           e.name,
           1,
           e.occurred_at,
           e.occurred_at,
           CASE WHEN e.name = 'bench_order_placed'
                THEN jsonb_build_object(
                         'amount',   round((random() * 200)::numeric, 2),
                         'category', (ARRAY['books', 'toys', 'garden', 'sports', 'beauty', 'food'])[1 + (random() * 5)::int],
                         'quantity', 1 + (random() * 4)::int)
                ELSE '{}'::jsonb END,
           'api',
           NULL,
           gen_random_uuid(),
           'valid',
           '[]'::jsonb,
           '{}'
    FROM (
        SELECT g, random() AS u, random() AS t FROM generate_series(lo, hi) g
    ) r
    CROSS JOIN LATERAL (
        SELECT CASE
                   WHEN r.u < 0.40 THEN 'bench_page_viewed'
                   WHEN r.u < 0.60 THEN 'bench_item_added'
                   WHEN r.u < 0.85 THEN 'bench_order_placed'
                   WHEN r.u < 0.90 THEN 'bench_plan_changed'
                   ELSE 'bench_support_opened' END AS name,
               date_trunc('second', seed_ts - r.t * make_interval(days => n_days)) AS occurred_at
    ) e
$$;

SELECT :'ws'::uuid AS ws, :'seed_at'::timestamptz AS seed_ts, :people::bigint AS n_people,
       :events::bigint AS n_events, :days::int AS n_days, :chunk::bigint AS chunk,
       least(:lg_pool::bigint, :people::bigint) AS pool
\gset

\echo seeding :n_people people
SELECT format('SELECT pg_temp.seed_people(%L, %s, %s, %L)', :'ws', lo, least(lo + :chunk - 1, :n_people), :'seed_ts')
FROM generate_series(1, :n_people, :chunk) AS lo
\gexec

-- The identities loadgen's userId resolves to: kind `external:user_id`, the value as sent.
INSERT INTO identities (id, workspace_id, person_id, kind, value)
SELECT md5(:'ws' || '/i/' || g)::uuid, :'ws'::uuid, md5(:'ws' || '/p/' || g)::uuid,
       'external:user_id', 'bench-u-' || (g - 1)
FROM generate_series(1, :pool) g;

\echo seeding :n_events events
SELECT format('SELECT pg_temp.seed_events(%L, %s, %s, %s, %s, %L)', :'ws', lo,
              least(lo + :chunk - 1, :n_events), :n_people, :n_days, :'seed_ts')
FROM generate_series(1, :n_events, :chunk) AS lo
\gexec
