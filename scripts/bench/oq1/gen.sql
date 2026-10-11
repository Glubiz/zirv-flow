-- OQ-1 data: :people rows of 20 mixed-type traits in a table `raw`, generated with a fixed seed
-- so both variants get the same values. Expects :schema and :people (psql variables).
\set ON_ERROR_STOP on
DROP SCHEMA IF EXISTS :"schema" CASCADE;
CREATE SCHEMA :"schema";
SET search_path = :"schema", pg_catalog;

SELECT setseed(0.42);

-- Six strings, five numbers, four booleans, three timestamps or dates, two string lists. Trait
-- values are stored the way the profile store stores them: timestamps and dates as ISO text.
CREATE TABLE raw AS
SELECT g::bigint AS id,
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
       ) AS traits
FROM generate_series(1, :people) g;
