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

## Lock review through 0112

This review covers every migration added in M8 and after, from `0060` through `0112`; there are no `0076`,
`0080` or `0086` files. Paths are relative to `crates/zirv-flow-store/migrations/`. Every merged
version is published as a release. As of 2026-11-12 the latest is `v0.286.0`, which includes
every migration through `0095`. An upgrade from an older release replays the files after its last
one: from `v0.214.0`, which ends at `0088`, it replays `0089` through `0112`, and from `v0.187.0`
it replays `0084` through `0112`. The drill selects the latest published older
stable release from GitHub at runtime, independently of local git tags. Its replay range can
change as releases are published.

**Decision: keep all these migrations and stop all roles for the upgrade.** Do not change the
SQL or checksums of released files. Keep the released `0083`, which creates an empty table.
`0084` adds a nullable column without a default or backfill and needs no rewrite.
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
| `0094_events_scan_indexes.sql` | Drops and recreates `events_name_idx` (adding `INCLUDE (person_id)`) and `events_person_idx` (appending `name` as the last key column) on the partitioned `events` parent, so both are rebuilt on every partition, then sets the insert autovacuum options on every partition. The two `DROP INDEX` statements take ACCESS EXCLUSIVE on `events` and every partition, held to the commit because sqlx runs the file as one transaction: all event reads and writes (ingest, the explorer, segment refreshes, flow arrivals) wait until both indexes are rebuilt on all partitions. The builds take SHARE (confirmed in `pg_locks` on the test database, which showed ACCESS EXCLUSIVE and SHARE on `events` and on a partition mid-run); the autovacuum `ALTER TABLE ... SET` takes SHARE UPDATE EXCLUSIVE. Measured on PostgreSQL 16.15 with 10M synthetic events in 4 monthly partitions (2.1 GB heap, 500,000 people, 200 event names, one workspace): 16.2 s on the first run and 14.6 s on a repeat, with the data in the OS cache; the wait grows with the stored events and a heap read from disk adds its read time, so plan for the full time as downtime of all event traffic. There is no pre-build path: the migration drops and recreates both indexes by name without `IF NOT EXISTS`, so an index built beforehand under another name is not used and the same name is dropped anyway, and `CREATE INDEX CONCURRENTLY` is not allowed on a partitioned table. The migration header states index growth of about 29% (`events_name_idx`) and 34% (`events_person_idx`) on live-shaped data; the synthetic set did not reproduce that growth (707 MB to 718 MB and 733 MB to 717 MB in total over its partitions), so budget the header's figures. Keep, stop all roles and upgrade in a quiet window. |
| `0095_source_records_traits_index.sql` | One partial index, `source_records_traits_idx` on `source_records (workspace_id, person_id)` for the live records that hold traits, read by the trait recompute, the merge and `traits.recompute` (round 7e2). The build takes SHARE on `source_records` (confirmed in `pg_locks` on the test database) and holds it to the commit: every ledger write (the drain's profile records, sync and Kafka units, imports, contacts) waits through the build, which scans the whole table; reads go on. Measured on PostgreSQL 16 on a 6M-row test ledger (2.0 GB heap, 1.32M rows indexed into 35 MB): 1.0 to 1.2 s with the heap in the OS cache; a heap read from disk adds its read time. Keep and budget the build, or pre-build it before the stop under the same name (the migration keeps a valid index of that name with this definition, and drops and rebuilds any other, such as an invalid one a failed build left): `CREATE INDEX CONCURRENTLY source_records_traits_idx ON source_records (workspace_id, person_id) WHERE deleted_at IS NULL AND traits <> '{}'::jsonb;` (3.5 s on the same ledger). A concurrent build takes SHARE UPDATE EXCLUSIVE, which lets ingest run. The migration does not build concurrently itself: a `-- no-transaction` file would build under the migration advisory lock, and a second migrator waiting for that lock (`database.migrate_on_start` on two roles) deadlocks the build, which PostgreSQL 16 ends with `deadlock detected` and an invalid index (reproduced on the test database). |
| `0096_empty_source_record_tombstones.sql` | Data only: one `UPDATE` of the tombstoned `source_records` (`deleted_at IS NOT NULL`) that still hold `traits`, `identifiers` or a `content_hash`, emptying them (the code now does so when a record is removed). Row locks on those rows only, ROW EXCLUSIVE on the table (no schema lock, writes to other rows go on); no index covers tombstones, so it writes only their heap rows. A deployment with no source-side deletions updates nothing. Keep. |
| `0097_decision_person_subject_indexes.sql` | Four partial expression indexes on the partitioned `decision_records` parent (`other_person`, `previous_person`, `survivor`, `merged`), so `delete_person` finds a person's decisions by probe instead of scanning every decision of the workspace. CREATE INDEX CONCURRENTLY is not allowed on a partitioned table, so each build takes SHARE on `decision_records` and every partition, held to the commit: all decision writes (ingest, flow steps, sends) wait through the builds; reads go on. The indexes hold only identity and merge decisions, so they are small and cost no other insert anything. Measured on PostgreSQL 16 with 1.5M synthetic decisions in 11 partitions (one workspace, no row carrying these keys): 0.7 s; the build grows with the stored decisions. Keep and budget the build in a quiet window. |
| `0098_jobs_person_enrollment_indexes.sql` | Four indexes on `jobs` for the job purge of an erasure or orphan delete, which seq-scanned `jobs` in about six statements per person. `jobs_person_idx` (workspace and payload `person_id`) and `jobs_enrollment_idx` (workspace and payload `enrollment_id`) are partial on `state = 'dead'`, so no ready or running row enters them and enqueue and claim pay nothing; `jobs_merged_person_idx` (pending `person.merge.rewrite` jobs) and `jobs_enroll_kind_idx` (the `flow.enroll` jobs of a workspace) cover low-volume kinds. The purge reaches the live rows through the queue's own indexes by kind (`jobs_ready_kind_idx`, `jobs_workspace_running_idx`), so it costs the ready backlog of the person-carrying kinds (`webhook.call`, `delivery.send`, `delivery.sync`, `template.test_send`, `flow.step`), not the table. `jobs` is a plain table but sqlx runs the file in one transaction, so no build is concurrent: each takes SHARE on `jobs` to the commit, and every enqueue, claim, heartbeat and completion waits through the four builds. Measured on PostgreSQL 16 with 1.5M jobs (462 MB heap, 1.4M of them dead, 30 MB per dead index): 1.5 s. Queue cost, measured on the same table with a 200,000-row enqueue, claim and complete loop (insert, update to running, delete, one statement each), medians of three runs: 3.55 s, 2.83 s and 0.32 s without the indexes against 3.75 s, 3.09 s and 0.37 s with them, so within about 10%. Keep, stop all roles and budget the build; the file creates the indexes without `IF NOT EXISTS`, so an index built beforehand under one of these names makes the migration fail. |
| `0099_source_records_contact_key_c_idx.sql` | One partial index, `source_records_native_contact_key_idx` on `source_records (workspace_id, (record_key COLLATE "C"))` for live native contacts, which serves the contact key-prefix search as a byte range. The build takes SHARE on `source_records` to the commit: every ledger write (the drain's profile records, sync and Kafka units, imports, contacts) waits; reads go on. Measured on PostgreSQL 16 with 1M native contacts (74 MB index): 0.7 s. The search now returns matches in byte order (`B1` before `a1`) instead of the database collation. Keep and budget the build in a quiet window. |
| `0100_ai_calls_pending_index.sql` | One partial index, `ai_calls_pending_idx` on `ai_calls (workspace_id, created_at)` for the `pending` calls, read by the abandon `UPDATE` that every `reserve_ai_call` runs under the workspace lock (it read the workspace's whole call history before). The build takes SHARE on `ai_calls` until the commit, so reservations and call records wait for it; reads go on. Measured on PostgreSQL 16 on 870,000 calls, 500,000 in one workspace: 71 ms. The abandon `UPDATE` went from a sequential scan (600,000 rows removed by the filter, 9,837 buffers, 39.7 ms) to an index scan on the new index (2 buffers, 0.04 ms). The index lists only pending rows, so it stays small. Write path of a call (insert pending, then the finishing `UPDATE`, each its own transaction, 10,000 calls, three runs each): 1.58 ms per call with and without the index. Keep. |
| `0101_jobs_consume_run_idx.sql` | One partial expression index, `jobs_consume_run_idx` on `jobs ((payload ->> 'run_id'))` for `kind = 'kafka.consume'` in `ready` or `running`, which the orphaned-run reaper (every minute) uses to find a consumer run's live job; before it, the reaper read the whole `jobs` table once per active run (1.5M jobs, 21 active runs: 10.9 s and 543k buffers per tick, now 0.4 ms and 87 buffers). It holds about one entry per active consumer run, so enqueue and claim of every other kind pay nothing: on 200,000-row insert and settle loops, medians of three, a mix with 1% consumer jobs took 1.73 s and 1.79 s without the index against 1.77 s and 1.80 s with it, and non-consumer jobs were unchanged. A table of nothing but consumer jobs (not a real shape) grew 31% and 40%. The build takes SHARE on `jobs` to the commit, so enqueues and claims wait through one scan of the table; pre-build it before the stop under the same name (the migration uses `IF NOT EXISTS`; drop an invalid index a failed build leaves, then retry): `CREATE INDEX CONCURRENTLY jobs_consume_run_idx ON jobs ((payload ->> 'run_id')) WHERE kind = 'kafka.consume' AND state IN ('ready', 'running');` |
| `0102_sync_runs_succeeded_idx.sql` | One partial index, `sync_runs_succeeded_idx` on `sync_runs (workspace_id, stream_id)` for succeeded sync runs, so the first-sync check of every event batch is one probe and not a read of the workspace's run history (200,000 runs: 18.5 ms and 5.7k buffers per batch, now 0.14 ms and 7 buffers). It adds one entry per successful run, never pruned; the settle of a run to `succeeded` pays for it once (200,000 settles: 1.66 s without, 2.08 s with, so about 2 microseconds a run). The build takes SHARE on `sync_runs`, blocking run starts and finishes through one scan; pre-build it before the stop under the same name (`IF NOT EXISTS`; drop an invalid index a failed build leaves): `CREATE INDEX CONCURRENTLY sync_runs_succeeded_idx ON sync_runs (workspace_id, stream_id) WHERE state = 'succeeded' AND run_kind = 'sync';` |
| `0103_messages_flow_node_status_idx.sql` | One partial index, `messages_flow_node_status_idx` on `messages (workspace_id, flow_id, flow_version, node_id, status, reason)` for the terminal messages of a flow, so the delivery counts of the canvas overlay are an index-only scan (660,000 messages: 65 ms and 16.8k buffers, now 38 ms and 613 buffers once vacuumed; 4 MB). A message enters it only when it ends, so the pending and sending churn does not touch it: on 200,000-row loops, medians of three, the insert took 3.83 s without and 3.62 s with it, and the settle to `sent` 4.00 s without and 4.34 s with it (8.5%). The build takes SHARE on `messages`, so every send's insert and settle waits through one scan; pre-build it before the stop under the same name (`IF NOT EXISTS`; drop an invalid index a failed build leaves): `CREATE INDEX CONCURRENTLY messages_flow_node_status_idx ON messages (workspace_id, flow_id, flow_version, node_id, status, reason) WHERE flow_id IS NOT NULL AND NOT test AND status IN ('sent', 'skipped', 'failed');` |
| `0104_message_tombstones.sql` | A new table, `message_tombstones` (about 150 bytes a row), and its person index. No lock on an existing table. The message-retention purge writes one row for each `sent` message it deletes, in the same statement, so the unsubscribe and preference links of old mail still work (`find_tracked_message` falls back to it); click links stay 404. Messages purged before this release have no tombstone and their links stay 404; that cannot be recovered. Erasure deletes a person's rows and the merge rewrite re-points them. |
| `0105_forwarding_outbox_dead_age_idx.sql` | One partial index, `forwarding_outbox_dead_age_idx` on `forwarding_outbox (updated_at)` for `dead` rows, read by the age purge the message-retention job now runs (`purge_forwarding_outbox`: dead rows older than the workspace's message window, and delivered rows of archived destinations). The build takes SHARE on `forwarding_outbox` until the commit, so enqueues and settles wait; reads go on. It scans the table once and writes only the dead rows, which are few. A dead row that is purged can no longer be re-driven. |
| `0106_messages_template_version_idx.sql` | One partial index, `messages_template_version_idx` on `messages (workspace_id, template_id, template_version) WHERE NOT test`, for the per-version stats of a template. The build takes SHARE on `messages` until the commit, so every message insert and update waits; reads go on. Budget it at the installation's message count. Measured on PostgreSQL 16 with 1.1M messages of one template, version 1 being 20,000 of them: the stats read went from a sequential scan (28,948 buffers, 94.8 ms) to an index scan (547 buffers, 6.8 ms). Write path of a message (insert pending, then the completing upsert, 5,000 messages, three runs each, medians): 7.75 s without the index and 7.87 s with it, +1.5 %, inside the run-to-run spread (7.68 to 8.69 s). Keep. |
| `0107_messages_person_id_idx.sql` | One index, `messages_person_id_idx` on `messages (person_id)`, so the foreign-key check of a person delete (`messages_person_id_fkey`, `ON DELETE CASCADE`) finds the person's rows by an index; every other `messages` index leads with `workspace_id`. Measured on PostgreSQL 16 with 1,000,000 messages of 10,000 people: the cascade trigger of one person delete took 1.08 ms without the index and 0.12 ms with it (the issue measured 145 ms for 3,000,000 messages over several workspaces). The build takes SHARE on `messages` until the commit, so every message insert and update waits; reads go on. Budget it at the installation's message count. Write path of a message (insert pending, then the completing update, 8,000 messages per run, 1,000,000 rows in the table, three runs each, medians): 17.55 s without the index and 17.30 s with it, inside the run-to-run spread (17.26 to 17.86 s). Keep. Pre-build before the stop under the same name: `CREATE INDEX CONCURRENTLY IF NOT EXISTS messages_person_id_idx ON messages (person_id);` (drop an invalid index a failed build leaves). |
| `0108_delivery_attempts_attempted_at_idx.sql` | One index, `delivery_attempts_attempted_at_idx` on `delivery_attempts (attempted_at)`, which serves the age purge the message-retention job now runs (`purge_attempts`: attempts older than the longest message window, 5,000 rows a batch, each batch committing alone; an attempt names no person). Before it, nothing deleted attempts. The build takes SHARE on `delivery_attempts` until the commit, so every send waits to record its attempt; reads go on. Write path (insert of an attempt, 15,000 inserts per run on a table of 1,000,000 rows, three runs each, medians): 15.79 s without the index and 16.14 s with it (+2.2 %, run-to-run spread 15.63 to 16.18 s). Keep. Pre-build before the stop under the same name: `CREATE INDEX CONCURRENTLY IF NOT EXISTS delivery_attempts_attempted_at_idx ON delivery_attempts (attempted_at);` The first run of the job after the upgrade deletes the whole backlog older than the window in batches. |
| `0109_template_use_daily.sql` | A new table, `template_use_daily (workspace_id, template_id, day, uses)`, filled by the existing `summary.rollup` job beside `summary_daily`, and read by the library's "most used" sort instead of a live `count(*)` over `messages`. No lock on an existing table. Visible change: the sort counts the last 30 whole UTC days (today and the 29 before), not the last 30 x 24 hours, and trails the messages by the job's five-minute period. The migration also clears every workspace's rollup marks (`summary_state`), so the job recounts today and backfills the last 30 days, a few days per run (under an hour), for both `template_use_daily` and `summary_daily`: "most used" ranks by the days counted so far until then, and the Home summary keeps its older rows until each day is recounted. One `UPDATE` over the small `summary_state` table, a row per workspace. The job's per-day count reads `messages_template_idx` (workspace, template, created time). Measured on PostgreSQL 16 with 667,000 messages of one template: the live count took 36 to 38 ms warm (the review measured 1,058 ms cold on a larger table), the rollup read of the library 2 to 5 ms. |
| `0110_flows_draft_revision.sql` | Adds `draft_revision integer NOT NULL DEFAULT 1` to `flows`, which counts the writes to a flow's draft so a save made from an older read is refused (409 `flow_draft_revision_mismatch`). A constant default is metadata-only on PostgreSQL 11 and later: no rewrite and no scan. ACCESS EXCLUSIVE on `flows` for an instant, waiting behind transactions that already use the table, which has one row per flow. No index. Keep. |
| `0111_webhooks_headers_differ.sql` | One CHECK on `webhooks`, `webhooks_headers_differ_check` (`timestamp_header IS NULL OR lower(timestamp_header) <> lower(signature_header)`), so two concurrent PATCHes that each passed the API's check cannot store equal header names (compared without regard to case); the API answers the loser with the same 422 as the check. ACCESS EXCLUSIVE on `webhooks` for an instant catalog change: the constraint is added `NOT VALID`, so no row is scanned and the migration never fails. It is enforced on every insert and update; a stored row that already has equal headers (the race can have left one) keeps working until it is next changed, when the 422 asks for a different header. Keep. |
| `0112_delivery_attempts_destination_covering_idx.sql` | Replaces `delivery_attempts_destination_idx` with the same key `(workspace_id, destination_id, attempted_at)` plus `INCLUDE (outcome, status_class)`, keeping the name, so the destination stats count is an index-only scan and not a read of every attempt row. The build takes SHARE on `delivery_attempts` until the commit, so every send waits to record its attempt; reads go on; the swap (drop and rename) is an instant ACCESS EXCLUSIVE. Measured on PostgreSQL 16 with 1,500,000 attempts (1,000,000 of the destination, all in the last 168 hours): the migration took 0.9 s; the stats query went from 29,564 buffers (bitmap heap scan, seqscan off; 24,559 as a sequential scan) to 10,927 buffers, Heap Fetches 0 after a vacuum (Index Only Scan), with the time on this machine about unchanged (162 ms before and 190 ms after, warm; the count of 1,100,000 rows dominates), so the cost is cut, not removed. The planner still prefers a sequential scan when the window covers about two thirds of the table. Index size 111 MB before, 117 MB after (all indexes of the table 370 MB and 391 MB). Write path (insert of an attempt, 15,000 autocommit inserts per run on a table of 1,000,000 rows, medians of three runs): 11.35 s without the change (10.95 to 12.48 s) and 12.39 s with it (11.95 to 14.22 s; a second set of three: 11.89 to 12.30 s), about +9 %, because the entries are wider; the number of indexes is unchanged. Keep. Pre-build before the stop (the file keeps an index of this name only when it is valid and exactly this index, so it then only drops and renames; an invalid or different one, such as a failed build leaves, is dropped and built again by the file): `CREATE INDEX CONCURRENTLY IF NOT EXISTS delivery_attempts_destination_stats_idx ON delivery_attempts (workspace_id, destination_id, attempted_at) INCLUDE (outcome, status_class);` |

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
