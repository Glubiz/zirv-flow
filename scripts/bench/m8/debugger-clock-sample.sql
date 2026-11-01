-- One host/database clock bracket for debugger-latency.sql: client before, database now, client after.
\set client_before_us `python3 -I -c "import time; print(time.time_ns() // 1000)"`
SELECT extract(epoch FROM now()) * 1000000 AS db_now_us
\gset
\set client_after_us `python3 -I -c "import time; print(time.time_ns() // 1000)"`
INSERT INTO debugger_clock_samples VALUES (:'client_before_us'::numeric, :db_now_us, :'client_after_us'::numeric);
