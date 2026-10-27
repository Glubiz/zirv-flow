# Restore drill evidence

Run of `scripts/drill/restore.sh` on the development machine (2026-10-27), against PostgreSQL 16 in a
container, with the tools run through `docker exec`. The log holds counts and durations only. Rerun
it with:

```sh
ZF_DRILL_PG_PREFIX="docker exec -i <container>" \
ZF_DRILL_APP_BASE=postgres://<user>:<password>@127.0.0.1:<host port> \
ZF_DRILL_TOOL_BASE=postgres://<user>:<password>@127.0.0.1:5432 \
bash scripts/drill/restore.sh
```

- zirv-flow: zirv-flow 0.178.0
- PostgreSQL server: 16.15 (Debian 16.15-1.pgdg13+2)
- pg_dump: pg_dump (PostgreSQL) 16.15 (Debian 16.15-1.pgdg13+2)
- pg_restore: pg_restore (PostgreSQL) 16.15 (Debian 16.15-1.pgdg13+2)

## 1. Prepare the source database

- PASS people in the source before the backup (3)
- took 7s

## 2. Back up with pg_dump

- PASS the dump is not empty (yes)
- took 2s

## 3. Erase one person after the backup

- PASS the erasure request is accepted (202)
- PASS ledger files written (1)
- PASS people in the source after the erasure (2)
- took 7s

## 4. Restore with pg_restore into a fresh database

- PASS people in the restored database (3)
- PASS the erasure is missing from the restored ledger (0)
- took 5s

## 5. ANALYZE, then replay the erasures from the ledger directory

- replay-erasures output: `replayed 1 erasure request(s): 1 suppression(s) added, 1 person(s) erased, 0 pending inbox row(s) removed; 1 ledger entr(ies) merged, 0 skipped for an unknown workspace`
- took 2s

## 6. Assertions through SQL

- PASS the erased person is gone (0)
- PASS the erased person's identities are gone (0)
- PASS erased suppressions (1)
- PASS completed ledger entries (1)
- PASS the other people are intact (2)
- PASS their identities are intact (2)
- took 8s

## 7. Assertions through the API

- PASS /readyz (200)
- PASS the erased person is 404 (404)
- PASS a surviving person is 200 (200)
- took 1s

## Result

- assertions passed: 16, failed: 0
- total duration: 35s
- scratch databases created and dropped: 2
