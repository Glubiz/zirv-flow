-- One host/database clock bracket for debugger-latency.sql: client before, database now, client after.
\if :{?container_mode}
\set client_before_us `date +%s%6N`
SELECT extract(epoch FROM now()) * 1000000 AS db_now_us
\gset
\set client_after_us `date +%s%6N`
\else
\set client_before_us `python3 -I -c "import time; print(time.time_ns() // 1000)"`
SELECT extract(epoch FROM now()) * 1000000 AS db_now_us
\gset
\set client_after_us `python3 -I -c "import time; print(time.time_ns() // 1000)"`
\endif
INSERT INTO debugger_clock_samples VALUES (:'client_before_us'::numeric, :db_now_us, :'client_after_us'::numeric);
