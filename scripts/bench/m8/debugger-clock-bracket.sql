-- The client-to-database clock offset before the debugger watcher starts (best of ten brackets).
-- debugger-latency.sql compares it with the bracket taken after the run: the offset must not drift.
-- Run it on the same host (or container) as the watcher: --csv -v container_mode=1 in the container.
\set ON_ERROR_STOP on
CREATE TEMP TABLE debugger_clock_samples (before_us numeric, db_now_us numeric, after_us numeric);
\ir debugger-clock-sample.sql
\ir debugger-clock-sample.sql
\ir debugger-clock-sample.sql
\ir debugger-clock-sample.sql
\ir debugger-clock-sample.sql
\ir debugger-clock-sample.sql
\ir debugger-clock-sample.sql
\ir debugger-clock-sample.sql
\ir debugger-clock-sample.sql
\ir debugger-clock-sample.sql
SELECT (before_us + after_us) / 2 - db_now_us AS offset_us, (after_us - before_us) / 2 AS uncertainty_us
FROM debugger_clock_samples ORDER BY after_us - before_us LIMIT 1;
