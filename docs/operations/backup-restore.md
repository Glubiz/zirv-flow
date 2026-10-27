# Backup and restore

Read this top to bottom when you have to restore. The design is in `docs/architecture.md` sections
16.3 and 17. The rehearsal that proves these steps is `scripts/drill/restore.sh`, with its log in
`docs/operations/evidence/restore-drill.md`.

## What to back up

Four things, and the last two are backed up apart from the database:

| What | Where | Without it |
| --- | --- | --- |
| The database | PostgreSQL | nothing to restore |
| The master key | `secrets.master_key_file` | every secret is unreadable and the HMAC root cannot be opened |
| The config file | the `--config` file or your `ZIRV_FLOW_*` environment | you must rebuild the settings by hand |
| The ledger directory | `privacy.ledger_dir` | erased people come back with the database and nothing can erase them again |

A database backup alone cannot decrypt secrets and cannot keep erased people erased. Keep the key
and the ledger directory on storage that a restore of the database does not roll back, and never
in the same archive as the dump.

## How to take the backup

**Small installs: `pg_dump`.** Run it with the services running or stopped; both are consistent.

```sh
pg_dump -Fc "$DATABASE_URL" > zirv-flow-$(date +%F).dump
```

Check: the file is not empty and `pg_restore --list zirv-flow-*.dump | head` prints a table of
contents.

**Production: WAL archiving and point-in-time recovery.** Settings to check on the server:
`wal_level = replica` (or higher), `archive_mode = on`, an `archive_command` that copies each
segment off the machine and returns non-zero on failure, and a base backup (`pg_basebackup`) taken
on a schedule. To recover, restore the base backup, set `restore_command`, set
`recovery_target_time` to a moment before the damage and `recovery_target_action = 'promote'`, and
start the server. Check: `SELECT pg_is_in_recovery()` returns `f` after the promote.

**PostgreSQL version.** Restore into the same major version as the dump, or a newer one; a dump
from a newer major cannot be restored into an older server. `pg_dump` must be the same major as
the server it dumps or newer. Physical (base backup and WAL) restores need exactly the same major.
zirv-flow needs PostgreSQL 16 or newer.

**The ledger directory** is already current: each erasure writes its file before it is reported
complete. Copy the directory on a schedule that is shorter than the time you accept to lose
(`cp -a` to other storage, or a snapshot). To fill it for erasures made before `privacy.ledger_dir`
was set, run `zirv-flow admin export-ledger --out <dir>` once.

## Restore, in this order

Do not skip or reorder steps 5 to 7. A restored database holds erased data until the replay has run,
so nothing may serve traffic before it.

1. **Stop all roles.** Stop every `serve`, `worker`, `scheduler` and `all` process.
   Check: `curl -s localhost:8080/readyz` fails to connect, and `SELECT count(*) FROM pg_stat_activity
   WHERE datname = '<db>'` is 0 apart from your own session.
2. **Put the key, config and ledger directory back.** Restore the master key file to the path in
   `secrets.master_key_file` (mode `0600`), the config file, and the ledger directory to
   `privacy.ledger_dir`. Check: `zirv-flow config check` exits 0.
3. **Restore the database** into an empty database.
   `pg_dump` backups:
   ```sh
   createdb "$DB" && pg_restore --no-owner --exit-on-error -d "$DB" zirv-flow-2026-01-01.dump
   ```
   Point-in-time recovery: see above. Check: `SELECT count(*) FROM people` is plausible.
   Abort: drop the new database and start step 3 again; the live database was not touched if you
   restored into a new name.
4. **Put the matching binary in place.** Use the release the backup was taken with, or a newer one.
   Never an older one. Check: `SELECT max(version) FROM _sqlx_migrations` is not above the
   migrations of your binary; if it is, startup refuses with a newer-schema error (see Where it
   fails). A newer binary applies its migrations with `zirv-flow migrate`.
5. **Run `ANALYZE` on the whole database.** The people lock query plan depends on fresh statistics
   (#297), and a restore starts with none. Check: `ANALYZE` finishes with no error; then
   `SELECT count(*) FROM pg_stat_user_tables WHERE last_analyze IS NULL AND last_autoanalyze IS NULL`
   falls to the tables that are empty.
6. **Replay the erasures from the ledger directory.**
   ```sh
   zirv-flow admin replay-erasures --from <ledger_dir>
   ```
   Check: it prints counts (`replayed N erasure request(s) ...`) and exits 0. Erasures made after
   the backup show as `ledger entr(ies) merged`. It is safe to run again. If it names a file, see
   Where it fails.
7. **Start the roles:** `serve`, `worker`, `scheduler` (or `all`). Only now.
   Check: `curl -fsS localhost:8080/readyz` returns 200.

## Checks after the restore

| Check | Command | Expect |
| --- | --- | --- |
| Config | `zirv-flow config check` | exit 0, secrets redacted |
| Ready | `curl -fsS localhost:8080/readyz` | 200 |
| Schema version | `SELECT max(version) FROM _sqlx_migrations` | the newest migration of the binary |
| One erased person is absent | `GET /api/v1/workspaces/<ws>/people/<id>` for a person in `SELECT person_id FROM erasure_requests WHERE status = 'completed' LIMIT 1` | 404 |
| Queue depth | `SELECT state, count(*) FROM jobs GROUP BY state` | no `dead` rows you did not expect; `ready` falls toward 0 |

## Where it fails

- **The master key is missing or wrong.** `serve`, `worker` and `all` refuse to start and name
  `secrets.master_key_file`. There is no way around it: without the key the secrets and the HMAC
  root are lost. Find the key backup; see `docs/operations/secrets.md`.
- **The ledger directory is older than the failure.** Erasures completed after the directory copy
  are in neither the directory nor the restored database, and the replay cannot know them. Compare
  the newest file in the directory with the newest erasure you know of, and re-request the missing
  ones. An older directory than the database backup is harmless: the database already holds those.
- **The ledger directory is missing.** Replay without `--from` still re-applies the ledger rows in
  the database, which only covers erasures made before the backup. Treat erasures after the backup
  as lost and re-request them.
- **A ledger file is corrupt or of an unknown format.** The replay names each such file on
  stderr, applies every other entry in the same run, and exits non-zero so you do not start
  serving unaware. Do not start any role yet: repair or re-request that erasure, move the bad
  file out of the directory, and run the replay again until it exits 0.
- **The schema is newer than the binary.** Startup refuses with a newer-schema message. Use a binary
  at least as new as the backup. Never edit `_sqlx_migrations` to get past it.
- **The workspace in a file does not exist here.** The file is skipped and counted as
  `skipped_workspaces`; this is expected when you restore a single workspace's database.
