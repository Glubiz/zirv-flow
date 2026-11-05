# Upgrade

An upgrade stops the whole stack. Migrations are embedded in the binary and run forward only.
There is no rolling or mixed-version upgrade; that is OQ-17. Rollback means restoring the backup
and its matching binary, master key, config and erasure ledger. Never remove migration records or
edit an applied migration to make an older binary start.

## Before stopping

Read the [backup and restore procedure](backup-restore.md) (#398). Have a tested backup, the master
key and ledger backups, the old image, the new image and database administrator access ready.
Name the operator responsible for the restore and agree the maintenance window with them. If a
step fails or the window expires, keep all roles stopped and give that operator the failed step,
exit code and migration number. Do not send database contents or identifier values with the report.

Rehearse on a restored copy with the same PostgreSQL major version, storage and row counts. The
small CI drill below proves correctness; it does not size a production maintenance window. Allow
time for the backup, the migration, verification, and a full restore if verification fails.

## Upgrade, in this order

1. **Back up.** Follow the backup guide, including the separate master key, config and ledger.
   Check that the dump has a readable table of contents and that its restore has been rehearsed.
2. **Stop all roles and incoming traffic.** Stop every `serve`, `worker`, `scheduler` and `all`
   process, including replicas. For the quickstart, `docker compose stop zirv-flow` leaves
   PostgreSQL and the volumes running. Stop producers or keep their requests for retry.
   Check that `/readyz` no longer answers and that no application sessions remain in
   `pg_stat_activity`. A backup taken before the stop omits later writes; take the final backup
   after the stop if rollback must preserve every accepted write.
3. **Migrate with the new binary.** Keep the same database and key. Run `zirv-flow migrate` once,
   with the timeouts below. It exits 0 and prints `applied migration N` for each applied file, or
   `database schema is up to date` when none is pending. Run it again: expect the latter message
   and exit 0. If either run fails, leave the stack stopped. Earlier files may already have
   committed; resolve the reported cause and retry with the same binary, or restore the backup.
4. **Start the new version everywhere.** Start `serve`, `worker` and `scheduler`, or `all`.
   Leave `database.migrate_on_start` off for this procedure. Startup refuses pending migrations,
   a partially applied migration, an applied file whose checksum changed, or an unknown schema
   version. Check `curl -fsS localhost:8080/readyz` and `curl -fsS localhost:8080/api/v1/version`.
   The latter's `version` must be the release you deployed. Open the web UI too.
5. **Verify before opening traffic.** Check a surviving and an erased person through
   `GET /api/v1/workspaces/{ws}/people/{id}`: expect 200 and 404. Refresh a segment, wait for a
   waiting enrollment to finish, and send a template test to an approved recipient. The drill
   below exercises these steps. If any check fails, stop the roles again and restore through
   the backup guide, including erasure replay, before allowing traffic.

`database.migrate_on_start = true` also migrates under the migration advisory lock. It does not
make mixed versions safe. Use the explicit command during a planned upgrade so the migration
finishes before any role starts. See [Requirements and database migrations](../../README.md#requirements-and-database-migrations).

## Timeouts and locks

`migrate` has no timeout flags. Set PostgreSQL `lock_timeout` and `statement_timeout` on its
connection through the `options` query parameter in `database.url`. For a URL with no query:

```sh
ZIRV_FLOW_DATABASE__URL="${DATABASE_URL}?options=-c%20lock_timeout%3D5s%20-c%20statement_timeout%3D30min" \
  zirv-flow migrate
```

Use `&options=` if the URL already has a query. Preserve its existing options. Five seconds and
30 minutes are examples, not measured limits: choose the statement timeout from the rehearsal
and the agreed window. A file runs as a transaction and holds its locks until commit. A timeout
aborts that file; it does not undo earlier files. The statement timeout applies to a statement,
not to the entire upgrade. Do not use an unlimited wait to hide an unexplained lock holder.

With all roles stopped, a lock wait is not application traffic. Check other administrative
sessions, open transactions, backups and another migrator in `pg_stat_activity` and `pg_locks`.
End the conflicting operation deliberately before retrying. Do not kill sessions blindly.

## Lock review through 0091

This review covers every migration added in M8, from `0060` through `0091`; there are no `0076`,
`0080` or `0086` files. Paths are relative to `crates/zirv-flow-store/migrations/`. Every merged
version is published as a release. As of 2026-11-01 the latest is `v0.214.0`, which includes
every migration through `0088`; an upgrade from an older release replays the files after its last
one (from `v0.187.0`, `0084` through `0088`, and `0089` through `0091` from later releases). The drill selects the latest published older
stable release from GitHub at runtime, independently of local git tags. Its replay range can
change as releases are published.

**Decision: keep all these migrations and stop all roles for the upgrade.** Do not change the
SQL or checksums of released files. Keep the released `0083`, which creates an empty table.
The unreleased `0084` adds a nullable column without a default or backfill and needs no rewrite.
No listed migration requires a populated heap rewrite. CHECK validation and ordinary index builds still
scan populated tables and can dominate downtime. New foreign keys also briefly lock their
referenced tables, even when the new child table is empty.

| Migration | Work and locks on populated tables; keep decision |
| --- | --- |
| `0060_assets.sql` | New asset tables and indexes. Foreign keys lock `workspaces` and `users`; no populated asset scan. Keep. |
| `0061_themes.sql` | New theme tables, indexes, deferred pointer and triggers. Foreign keys lock `workspaces` and `users`; no populated theme scan. Keep. |
| `0062_snippets.sql` | New snippet tables, indexes, deferred pointer and triggers. Foreign keys lock `workspaces` and `users`; no populated snippet scan. Keep. |
| `0063_message_versions.sql` | Adds nullable columns and CHECKs to `messages` and `test_sends`. ACCESS EXCLUSIVE; allow for CHECK validation over both tables. Keep for the stopped window. |
| `0064_template_preview_people.sql` | New table and index. Foreign keys lock `workspaces`, `people`, `users` and `templates`; no populated child scan. Keep. |
| `0065_ai_template_drafts.sql` | Alters `ai_drafts`, replaces and adds CHECKs, adds a foreign key to `templates` and builds an index. ACCESS EXCLUSIVE plus validation and index scans. Keep for the stopped window. |
| `0066_test_send_draft_revision.sql` | Adds a nullable column and CHECK to `test_sends`, then a partial index. ACCESS EXCLUSIVE through validation and build. Keep for the stopped window. |
| `0067_template_draft_checks.sql` | Nullable column on `template_drafts`, no default or backfill. Brief ACCESS EXCLUSIVE catalog change. Keep. |
| `0068_template_version_note.sql` | Nullable note and length CHECK on `template_versions`. ACCESS EXCLUSIVE; allow for validation. Keep for the stopped window. |
| `0069_events_id_index.sql` | Index on `events` and every partition. Ordinary builds block writes for their duration. Keep and budget the full build over the retained events. |
| `0070_jobs_ready_kind_index.sql` | Partial index on `jobs`. SHARE blocks writes through the build. Keep for the stopped window. |
| `0071_messages_retention.sql` | Index on `messages` takes SHARE through the build; inserts a retention schedule. Keep for the stopped window. |
| `0072_enrollments_live_idx.sql` | Partial index on `enrollments`, SHARE through the build. Keep for the stopped window. |
| `0073_membership_roles.sql` | Replaces `memberships_role_check` under ACCESS EXCLUSIVE, maps `member` to `editor`, validates, and adds `publish` to existing admin API keys. Row updates also lock the affected rows. Keep; include both scans in the window. |
| `0074_debugger_epoch.sql` | New unlogged singleton table. No populated table alteration. Keep. |
| `0075_decision_retention.sql` | Inserts a schedule if absent. Locks the inserted schedule row; no populated table DDL. Keep. |
| `0077_audit_read_api.sql` | Nullable column, six ordinary indexes and guard triggers on `audit_log`. ACCESS EXCLUSIVE from the column remains through the index builds; audited writes stop. Keep and budget all six builds. |
| `0078_membership_view_pii.sql` | Adds `memberships.view_pii` with constant default `false`. PostgreSQL 16 can store the default without rewriting rows; ACCESS EXCLUSIVE is still required. Keep. |
| `0079_jobs_audit_retention.sql` | Inserts two schedules if absent. Row locks, no populated table DDL. Keep. |
| `0081_retention_settings.sql` | New settings and run tables. Foreign keys lock `workspaces` and `users`; no populated child scan. Keep. |
| `0082_drop_trait_change_cause.sql` | Checks for forbidden existing causes, then replaces four CHECKs on `jobs`, `decision_records` including partitions, `person_changes`, and `enrollments`. ACCESS EXCLUSIVE lasts through every validation scan. Keep for the stopped window. A pre-existing `trait_change` aborts the migration; investigate it, do not delete it to pass. |
| `0083_api_idempotency.sql` | New empty table and index; foreign key locks `workspaces`. Keep. |
| `0084_forwarding_trace.sql` | Nullable `forwarding_outbox.trace_id`, no default, scan or backfill. Brief ACCESS EXCLUSIVE catalog change. Keep. |
| `0085_summary_rollup.sql` | Three new empty tables (their foreign keys briefly lock `workspaces`), three ordinary indexes and the `summary.rollup` schedule. No new `events` index: events are counted on `0069`'s. SHARE blocks writes through each index build. Keep and budget the builds over the retained rows, or pre-build each before the stop under the same name (the migration uses `IF NOT EXISTS`; drop an invalid index a failed build leaves, then retry): `CREATE INDEX CONCURRENTLY people_workspace_created_idx ON people (workspace_id, created_at);` `CREATE INDEX CONCURRENTLY messages_sent_completed_idx ON messages (workspace_id, completed_at) INCLUDE (flow_id) WHERE status = 'sent' AND NOT test;` `CREATE INDEX CONCURRENTLY step_executions_workspace_node_started_idx ON step_executions (workspace_id, node_id, started_at) INCLUDE (enrollment_id, outcome);`. The rollup backfills 30 days newest first, three days per five-minute run and one transaction per day, so Home figures fill in over about an hour. |
| `0087_flow_warmup.sql` | Four new `flows` columns: two `NOT NULL` with constant defaults (a catalog change, no rewrite) and two nullable. ACCESS EXCLUSIVE on `flows` lasts through the scan that checks the new `warmup_seconds` CHECK; `flows` is small. New empty table `flow_warmup_waiting` with one index; its foreign keys briefly lock `people` and `flows`. Keep. |
| `0088_jobs_capped_claim_indexes.sql` | Two partial indexes on the ready `jobs` for the capped claim (#207). SHARE blocks enqueues and claims through each build, which scans the whole `jobs` table. Keep and budget the builds, or pre-build each before the stop under the same name (the migration uses `IF NOT EXISTS`; drop an invalid index a failed build leaves, then retry): `CREATE INDEX CONCURRENTLY jobs_ready_workspace_idx ON jobs (workspace_id, priority, run_at) WHERE state = 'ready' AND workspace_id IS NOT NULL;` `CREATE INDEX CONCURRENTLY jobs_ready_instance_idx ON jobs (priority, run_at) WHERE state = 'ready' AND workspace_id IS NULL;` |
| `0089_people_merged_into_index.sql` | One partial index on the merged `people` for the merge-group lookup (#397). SHARE blocks every write to `people` through the build, which scans the whole table. Keep and budget the build, or pre-build it before the stop under the same name (the migration uses `IF NOT EXISTS`; drop an invalid index a failed build leaves, then retry): `CREATE INDEX CONCURRENTLY people_merged_into_idx ON people (workspace_id, merged_into) WHERE merged_into IS NOT NULL;` |
| `0091_events_name_person_index.sql` | Replaces `events_name_idx` with `events_name_person_idx` (`workspace_id, name, occurred_at DESC` `INCLUDE (person_id)`), so the event side of a segment aggregate is an index-only scan (#397). The drop of the old index takes ACCESS EXCLUSIVE on `events` and every partition, blocking reads and writes; a partitioned index cannot be dropped CONCURRENTLY. It goes first and waits at most 2 s per attempt, ten attempts one second apart, then fails with SQLSTATE `55P03` and the whole file rolls back. Because the drop goes first, its lock lasts through every partition index the file builds itself, a scan of each partition: estimated at about 11 GB of index and 10-30 min over the M8 bench's 130 M events. Keep, and pre-build before the stop as in [Pre-building 0091](#pre-building-0091); the file then only checks that work and takes brief locks, and it resumes a manual run that stopped halfway. |

**Older installs: `0033_trait_slots.sql`.** This is already present in `v0.187.0`, so that upgrade
does not replay it. Keep its released SQL. It adds 30 nullable columns to `people`, builds 30
partial indexes in one transaction. It also adds columns, CHECKs and a unique partial index to
`trait_definitions`, including the constant default `0` for `slot_generation`; that table needs
ACCESS EXCLUSIVE through validation and its index build. The initial ACCESS
EXCLUSIVE lock lasts through the builds, blocking reads as well as writes to `people`. An older
install that still needs `0033` must include all 30 builds in its stopped window. This is the
decision replacing the original comment's requirement to revisit the build before supporting
populated upgrades. It does not enable online upgrades.

**Measured downtime.** No populated migration timing is recorded for this range or for `0033`.
Do not treat the existing ingestion, segment or restore benchmarks as migration timings. Each
CI run records its table counts, total migration wall time and sqlx per-file execution times in
the job summary. Its fixture has 3,000 people and 3,000 events across three monthly partitions,
then one erasure before the baseline. It does not measure a large audit log, message history or
the 30-index `0033` build. Record rehearsal counts and downtime on production-sized data before
committing to a maintenance window. Until a green CI run exists, the drill is unverified.

### Pre-building 0091

`0091` replaces the event name index. Left to the migration, the build of every partition index
falls inside the stopped window and lengthens it by the whole build. Build it before the stop
instead, while the stack runs, from `psql` on the primary as the application's database owner.
Each step can be run again, and a step that fails undoes nothing before it.

Budget about 11 GB of disk for the new index next to the old one until step 6, and 10-30 min for
step 1 over the M8 bench's 130 M events (two passes over each partition, about 50-60 GB read).
These are estimates from the #397 investigation, not measurements; rehearse on a restored copy.

1. **Build one index per partition, concurrently.** Each build takes SHARE UPDATE EXCLUSIVE on its
   partition: reads and writes continue, and the build first waits for the transactions already
   open on it, so run it without a lock timeout. `IF NOT EXISTS` skips the partitions already
   built:

   ```sql
   SELECT format('CREATE INDEX CONCURRENTLY IF NOT EXISTS %I ON %I '
                 '(workspace_id, name, occurred_at DESC) INCLUDE (person_id)',
                 c.relname || '_workspace_id_name_occurred_at_person_id_idx', c.relname)
   FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
   WHERE i.inhparent = 'events'::regclass ORDER BY c.relname \gexec
   ```

   A failed or cancelled build leaves an invalid index, which `IF NOT EXISTS` would then skip. List
   them, drop each with `DROP INDEX CONCURRENTLY <name>;` and run step 1 again:

   ```sql
   SELECT c.relname FROM pg_index x JOIN pg_class c ON c.oid = x.indexrelid
   WHERE c.relname LIKE 'events%_workspace_id_name_occurred_at_person_id_idx' AND NOT x.indisvalid;
   ```

2. **Bound every lock wait** from here on: the steps below block other sessions while they wait.

   ```sql
   SET lock_timeout = '2s';
   ```

3. **Create the parent index on `events` alone.** It takes SHARE on `events` only, until the
   statement commits: an insert waits for that moment, and the statement waits for the inserts in
   flight.

   ```sql
   CREATE INDEX IF NOT EXISTS events_name_person_idx
       ON ONLY events (workspace_id, name, occurred_at DESC) INCLUDE (person_id);
   ```

   From here on, a partition the scheduler creates gets its own index, attached. One created
   between steps 1 and 3 has none: run step 1 again.

4. **Attach each partition index.** Each attach takes ACCESS SHARE on `events` and the partition,
   SHARE UPDATE EXCLUSIVE on the parent index and ACCESS EXCLUSIVE on the partition index. Every
   open transaction that read or wrote the partition holds a lock on its indexes until it ends, so
   the attach waits for those and holds up new readers and writers of the partition while it
   waits, at most the lock timeout. Attaching an attached index only checks the parent again:

   ```sql
   SELECT format('ALTER INDEX events_name_person_idx ATTACH PARTITION %I',
                 c.relname || '_workspace_id_name_occurred_at_person_id_idx')
   FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
   WHERE i.inhparent = 'events'::regclass ORDER BY c.relname \gexec
   ```

5. **Check that the parent is valid**, which it is once every partition has a valid index
   attached. If not, find the invalid partition index with step 1's query and rebuild it in place
   (an attached index cannot be dropped) with `RESET lock_timeout;` and
   `REINDEX INDEX CONCURRENTLY <name>;`, then run steps 2 and 4 again:

   ```sql
   SELECT indisvalid FROM pg_index WHERE indexrelid = 'events_name_person_idx'::regclass;
   ```

6. **Drop the old index.** This is the one step that cannot be concurrent: it takes ACCESS
   EXCLUSIVE on `events` and every partition. Once granted it holds the lock for milliseconds, but
   while it waits it blocks every read and write of events, so each attempt waits at most 2 s,
   ten attempts one second apart, as the migration does:

   ```sql
   DO $$
   DECLARE attempt integer := 0;
   BEGIN
       PERFORM set_config('lock_timeout', '2s', true);
       LOOP
           attempt := attempt + 1;
           BEGIN
               DROP INDEX IF EXISTS events_name_idx;
               EXIT;
           EXCEPTION WHEN lock_not_available THEN
               IF attempt >= 10 THEN RAISE; END IF;
               PERFORM pg_sleep(1);
           END;
       END LOOP;
   END
   $$;
   ```

**If every attempt fails**, nothing changed: both indexes stay, and inserts keep writing both. At
each attempt a transaction had held a lock on `events` or a partition for more than 2 s, typically
a segment refresh, an explorer export, a preview or a backup. Find the holders, without query text:

```sql
SELECT DISTINCT a.pid, a.backend_type, a.state, a.xact_start
FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
WHERE a.pid <> pg_backend_pid()
  AND l.relation IN (SELECT 'events'::regclass
                     UNION ALL SELECT inhrelid FROM pg_inherits WHERE inhparent = 'events'::regclass)
ORDER BY a.xact_start;
```

Run step 6 again once it has ended, or leave the drop to `migrate`, which makes the same attempts in
the stopped window. If `migrate` fails on `0091` with SQLSTATE `55P03`, the file rolled back whole
and the stack stays stopped: with all roles stopped the holder is an administrative session, a
backup (`pg_dump` holds ACCESS SHARE on every table it dumps until it finishes) or another
migrator. Let it finish or end it deliberately, then run `migrate` again.

## Upgrade drill and release gate

CI's `upgrade-drill` job runs on PRs changing migrations, the drill or its workflow, on version
tags, and on demand. It uses `scripts/drill/upgrade.sh`, `scripts/drill/compose.upgrade.yaml` and
`scripts/drill/integrity.sql`. Only CI runs the containers. It creates an isolated compose project
and removes its volumes on exit.

The previous image is `ghcr.io/glubiz/zirv-flow:<version>` from the most recently published
non-draft, non-prerelease GitHub release older than the candidate. The leading `v` belongs to the
Git tag, not the image tag. `previous_tag` overrides the selection. No previous release or a
missing previous image manifest produces a skip notice; authentication and network errors fail.
A skip is not evidence for criterion 6.

The fixture creates the owner through setup and all application data through the public API.
An isolated DNS server serves the domain's requested records and a TLS SMTP sink counts accepted
messages. No domain verification or application rows are planted with SQL. One erasure completes
before the stopped baseline, so no later count loss is exempted. Counts include each table and
partition and are compared before roles restart: completed jobs are deleted during normal
operation. Constraints are validated and their rows checked, including already-validated foreign
keys. After restart the API checks erasure, segment refresh, the waiting enrollment, test delivery,
the web UI and the running version. Logs and summaries contain counts and durations, not personal
identifiers, bodies or raw database errors.

For a release candidate, dispatch the CI workflow with `image_tag` set to its published version
tag, such as `1.0.0-rc.1`. This pulls that image instead of building the checkout. Leave
`previous_tag` empty to choose the previous stable release. Keep the green run link and its
summary in the release checklist; [the release candidate procedure](../../CONTRIBUTING.md#release-candidates)
requires this gate. A locally built tag run does not substitute for testing the published rc.

An rc migrates the same way as a final release. A database with a migration unknown to an older
rc is refused by that rc. The check compares migration versions and checksums, not release-tag
precedence: two rc images with identical migrations are not distinguished by the schema check.
Do not use that as a rollback path; restore the backup for a downgrade.

The Docker-free count self-test is `bash scripts/drill/upgrade.sh --self-test`. CI also runs
`bash scripts/drill/upgrade.sh --self-test-postgres` with `DATABASE_URL` on PostgreSQL 16 and 18;
it proves an invalid CHECK and a broken foreign key fail. It creates and removes its own scratch
schema and needs a PostgreSQL superuser to plant the broken foreign key. The existing inline
store tests `migrating_twice_is_a_no_op` and
`a_newer_schema_is_detected_and_migration_is_refused` pin the migration behavior in the same CI
matrix. ShellCheck runs with the other script self-tests.
