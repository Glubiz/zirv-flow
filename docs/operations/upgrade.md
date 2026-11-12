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

## Lock review through 0095

This review covers every migration added in M8 and after, from `0060` through `0095`; there are no `0076`,
`0080` or `0086` files. Paths are relative to `crates/zirv-flow-store/migrations/`. Every merged
version is published as a release. As of 2026-11-12 the latest is `v0.286.0`, which includes
every migration through `0095`. An upgrade from an older release replays the files after its last
one: from `v0.214.0`, which ends at `0088`, it replays `0089` through `0095`, and from `v0.187.0`
it replays `0084` through `0095`. The drill selects the latest published older
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
| `0090_step_executions_hot_settle.sql` | Rebuilds the started index of `step_executions` without `outcome` as `step_executions_started_idx` and drops `step_executions_workspace_node_started_idx` and the unread `step_executions_workspace_idx`, so the settle of a wait is a HOT update when its page has room (#397; 59% of settles in a bench-shaped run, with the default fillfactor). It also replaces `enrollments_person_idx` (`workspace_id, person_id`) with `enrollments_person_id_idx` (`person_id`), so a person's delete finds its enrollments by index instead of reading the whole table or index for each person (#510). Each build takes SHARE on its table, blocking every flow step (and, on `enrollments`, every enrollment) through a scan of the whole table; each drop takes ACCESS EXCLUSIVE, blocking reads too, briefly once granted but queued behind any open reader. Keep and budget the builds, or do all five before the stop, outside a transaction and in this order, so the goal count of the rollup and the person reads always have an index (the migration uses `IF NOT EXISTS` and `IF EXISTS`; drop an invalid index a failed build leaves, then retry): `CREATE INDEX CONCURRENTLY step_executions_started_idx ON step_executions (workspace_id, node_id, started_at) INCLUDE (enrollment_id);` `DROP INDEX CONCURRENTLY IF EXISTS step_executions_workspace_node_started_idx;` `DROP INDEX CONCURRENTLY IF EXISTS step_executions_workspace_idx;` `CREATE INDEX CONCURRENTLY enrollments_person_id_idx ON enrollments (person_id);` `DROP INDEX CONCURRENTLY IF EXISTS enrollments_person_idx;`. A concurrent build and drop take SHARE UPDATE EXCLUSIVE, which lets flow steps and enrollments run, and each waits for the transactions already open on the table. |
| `0091_event_dedupe_person_index.sql` | One index on `event_dedupe (person_id)` for the merge rewrite, a person's delete and the foreign-key check of a `people` delete (investigation K). SHARE blocks every write to `event_dedupe`, so every event ingest, through the build, which scans the whole table. Keep and budget the build, or pre-build it before the stop under the same name (the migration uses `IF NOT EXISTS`; drop an invalid index a failed build leaves, then retry): `CREATE INDEX CONCURRENTLY event_dedupe_person_id_idx ON event_dedupe (person_id);` A concurrent build takes SHARE UPDATE EXCLUSIVE, which lets ingest run. |
| `0092_identities_person_fk_deferrable.sql` | Makes the foreign key of `identities` to `people` deferrable, still checked at each statement unless deferred (`DEFERRABLE INITIALLY IMMEDIATE`), so the resolution can claim a new person's identities before inserting it and a lost identity race no longer deletes a `people` row (perf P). Catalog only, no scan; it takes ACCESS EXCLUSIVE on `identities`, brief once granted but queued behind any open reader. Keep. |
| `0093_queue_autovacuum.sql` | Sets autovacuum storage parameters (fixed dead-tuple threshold, no cost delay, low analyze factor) on the plain tables `jobs`, `ingest_inbox` and `person_changes`, so their dead tuples are vacuumed promptly (perf R). Catalog only, no scan or rewrite; `ALTER TABLE ... SET` takes SHARE UPDATE EXCLUSIVE (confirmed in `pg_locks` on the test database), which blocks neither reads nor writes. Keep. |
| `0094_events_scan_indexes.sql` | Drops and recreates `events_name_idx` (adding `INCLUDE (person_id)`) and `events_person_idx` (appending `name` as the last key column) on the partitioned `events` parent, so both are rebuilt on every partition, then sets the insert autovacuum options on every partition. The two `DROP INDEX` statements take ACCESS EXCLUSIVE on `events` and every partition, held to the commit because sqlx runs the file as one transaction: all event reads and writes (ingest, the explorer, segment refreshes, flow arrivals) wait until both indexes are rebuilt on all partitions. The builds take SHARE (confirmed in `pg_locks` on the test database, which showed ACCESS EXCLUSIVE and SHARE on `events` and on a partition mid-run); the autovacuum `ALTER TABLE ... SET` takes SHARE UPDATE EXCLUSIVE. Measured on PostgreSQL 16.15 with 10M synthetic events in 4 monthly partitions (2.1 GB heap, 500,000 people, 200 event names, one workspace): 16.2 s on the first run and 14.6 s on a repeat, with the data in the OS cache; the wait grows with the stored events and a heap read from disk adds its read time, so plan for the full time as downtime of all event traffic. There is no pre-build path: the migration drops and recreates both indexes by name without `IF NOT EXISTS`, so an index built beforehand under another name is not used and the same name is dropped anyway, and `CREATE INDEX CONCURRENTLY` is not allowed on a partitioned table. The migration header states index growth of about 29% (`events_name_idx`) and 34% (`events_person_idx`) on live-shaped data; on the synthetic data the partition indexes went from 707 MB to 718 MB and from 733 MB to 717 MB in total, because the old indexes had been built by random-order inserts, so budget the header's figures. Keep, stop all roles and upgrade in a quiet window. |
| `0095_source_records_traits_index.sql` | One partial index, `source_records_traits_idx` on `source_records (workspace_id, person_id)` for the live records that hold traits, read by the trait recompute, the merge and `traits.recompute` (round 7e2). The build takes SHARE on `source_records` (confirmed in `pg_locks` on the test database) and holds it to the commit: every ledger write (the drain's profile records, sync and Kafka units, imports, contacts) waits through the build, which scans the whole table; reads go on. Measured on PostgreSQL 16 on a 6M-row test ledger (2.0 GB heap, 1.32M rows indexed into 35 MB): 1.0 to 1.2 s with the heap in the OS cache; a heap read from disk adds its read time. Keep and budget the build, or pre-build it before the stop under the same name (the migration keeps a valid index of that name with this definition, and drops and rebuilds any other, such as an invalid one a failed build left): `CREATE INDEX CONCURRENTLY source_records_traits_idx ON source_records (workspace_id, person_id) WHERE deleted_at IS NULL AND traits <> '{}'::jsonb;` (3.5 s on the same ledger). A concurrent build takes SHARE UPDATE EXCLUSIVE, which lets ingest run. The migration does not build concurrently itself: a `-- no-transaction` file would build under the migration advisory lock, and a second migrator waiting for that lock (`database.migrate_on_start` on two roles) deadlocks the build, which PostgreSQL 16 ends with `deadlock detected` and an invalid index (reproduced on the test database). |

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
