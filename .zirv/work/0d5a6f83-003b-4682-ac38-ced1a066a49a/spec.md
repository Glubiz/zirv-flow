# Spec: PostgreSQL pool and migrations (issue #4)

## Store crate (`zirv-flow-store`)

### Dependencies

`sqlx` 0.9 with these features:
- `postgres`, `migrate`, `macros`
- `runtime-tokio`
- rustls TLS on ring with native roots, so self-hosters' private CAs work. Pick the exact feature name for 0.9 and document it.

`build.rs` prints `cargo:rerun-if-changed=migrations`. Without it, a new migration file does not trigger a rebuild.

### Public surface, re-exported from `lib.rs`

- `PgPool` (re-exported).
- `connect(url: &str, max_connections: u32) -> Result<PgPool, StoreError>`.
- `migrate(&PgPool) -> Result<Vec<i64>, StoreError>`, returning the applied versions.
- `schema_state(&PgPool) -> Result<SchemaState, StoreError>`, where `SchemaState` is one of:
  - `UpToDate`
  - `Pending { count }`
  - `Newer { version }` (an applied version this binary lacks)
  - `Dirty { version }`
- A crate-internal variant of `migrate` and `schema_state` takes a `&Migrator`, so tests can run fixture migration sets.
- `StoreError` is the store's own error type with a `Display` impl. It must never include the connection URL.

### Migrations

- Embedded with `sqlx::migrate!()` from `crates/zirv-flow-store/migrations/`.
- The directory holds no migrations yet; the first tables arrive with #6 and #7. It is kept in git with a `.gitkeep`.
- Migrations are forward-only.
- `Migrator::run` keeps its default `pg_advisory_lock` and `ignore_missing = false`.

## Binary

- Depends on `zirv-flow-store`, which needs the 5.1 table change.
- Runs on a tokio runtime (`rt-multi-thread`, `macros`).
- Startup is a pure function, so it can be unit-tested without Postgres:
  `startup_action(state, migrate_on_start) -> Proceed | Migrate | Refuse(message)`.
  The refusal messages:
  - newer: `database schema version <v> is newer than this binary supports; upgrade zirv-flow`
  - pending, not migrating: `<n> pending migration(s); run \`zirv-flow migrate\` or set database.migrate_on_start`
  - dirty: `migration <v> is partially applied; repair the database and rerun \`zirv-flow migrate\``
- Role commands connect and gate as above. When the gate passes, the existing "not implemented yet" stub follows (exit 1).
- `migrate`:
  - connects;
  - refuses on a newer or dirty schema (exit 1);
  - otherwise migrates and prints either the applied versions or `database schema is up to date`, then exits 0.
- Exit codes:
  - 2 for config errors, as now;
  - 1 for database or startup failures, with stderr `error: database: <message>`.

## Tests

- Store tests that need Postgres carry `#[ignore = "requires PostgreSQL; set DATABASE_URL"]` and use `#[sqlx::test]`, which creates one database per test.
- Fixture migration sets live in `crates/zirv-flow-store/tests/fixtures/migrations-*/` (data only).
- Cases:
  - Migrating an empty database brings it up to date.
  - Migrating twice is a no-op.
  - A newer schema is detected, and `migrate` refuses it.
  - Pending migrations are counted.
  - A dirty row is detected.
  - Two concurrent migrators each apply every version once.
  - A connection failure with a wrong password does not echo the password.
- The binary's `startup_action` and its messages get plain unit tests.

## CI

- A new job, `PostgreSQL <16|18>`, with matrix `pg: [16, 18]`:
  - service `postgres:${{ matrix.pg }}` with a `pg_isready` health check;
  - `DATABASE_URL=postgres://postgres:postgres@localhost:5432/postgres`;
  - runs `cargo nextest run --run-ignored only --no-fail-fast`;
  - installs the toolchain and nextest the way the test job does.

## Docs

- `docs/architecture.md`:
  - §5.1: the binary row lists `zirv-flow-store`.
  - §5: the mermaid diagram gains `bin --> store`.
  - §17 Packaging: PostgreSQL 16+.
  - §21: OQ-15 is marked decided. Follow the file's existing convention for closed questions, or add a "Decided" note.
- `README.md`: requirements (PostgreSQL 16+), `zirv-flow migrate`, and how to run the PostgreSQL tests locally with Docker.
- `.zirv/context/common.md` module map: store now holds the pool and migrations. Regenerate CLAUDE.md and AGENTS.md.
- Commit the repo-bank entry `.zirv/memory/postgres-min-version-and-migrations.md`.

## Version

0.4.0.
