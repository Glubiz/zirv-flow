# Intent

## Problem

Issue #4. zirv-flow has configuration but no database: no pool, no migrations,
no `migrate` command and no startup guard against a schema the binary does not
match. OQ-15 (minimum PostgreSQL version) is still open.

## Desired outcome

- `zirv-flow-store` owns a sqlx Postgres pool and an embedded, forward-only
  migration stream.
- `zirv-flow migrate` applies pending migrations, with only one migrator at a
  time (an advisory lock).
- Role startup (`all`, `serve`, `worker`, `scheduler`) runs these steps in order:
  - Connect to the database.
  - If `database.migrate_on_start` is set, migrate.
  - Refuse to start, with a clear error, when:
    - the schema is newer than the binary;
    - migrations are pending and migrating is disabled;
    - a migration is dirty (partly applied).
- OQ-15 is decided: PostgreSQL 16 minimum, with CI against 16 and 18
  (supervisor ruling 95193475). It is recorded in the architecture and README.
- The binary may depend on `zirv-flow-store` (supervisor ruling 2ebf10ea). The
  5.1 table and diagram are amended so the binary builds one pool and hands it on.

## Constraints

- SQL and migration logic live in the store crate; the binary only coordinates.
- No error message prints the database password. Connection failures included.
- Tests that need PostgreSQL are opt-in locally and always run in CI.
- Version 0.4.0. No competing product is named anywhere.

## Open questions

None.

## Acceptance criteria

- [ ] `zirv-flow migrate` against an empty PG 16 or 18 database succeeds.
      Running it again is a no-op.
- [ ] Two concurrent migrators apply each version exactly once.
- [ ] A database with an applied version the binary lacks makes `migrate` and
      role startup exit 1 with a "newer than this binary" error.
- [ ] Pending migrations with `migrate_on_start = false` stop startup with an
      error that names `zirv-flow migrate` and `database.migrate_on_start`.
- [ ] A dirty migration stops startup.
- [ ] A connection failure does not echo the password.
- [ ] CI runs the PostgreSQL tests against 16 and 18.
- [ ] build, nextest, fmt, clippy, crate-deps and test-presence pass.
- [ ] OQ-15 is closed in docs/architecture.md.
