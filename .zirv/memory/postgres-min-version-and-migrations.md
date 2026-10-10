## Memory
- Key: postgres-min-version-and-migrations
- Written-by: claude
- Written: 1791658128
- Verified: 1791658128
- Source: explicit

OQ-15 decided (supervisor ruling 95193475, 2026-10-10): the minimum supported PostgreSQL is 16, and CI tests against 16 and 18. The decision is recorded in docs/architecture.md section 21 and the README. Reassess before v1.0 against the actual release date.

sqlx 0.9 facts (verified in sqlx-core 0.9.0 and sqlx-postgres 0.9.0 source):
- Migrator::run takes pg_advisory_lock by default (`locking: true`), so only one migrator runs at a time with no extra code.
- With `ignore_missing` false it returns MigrateError::VersionMissing when the database has an applied migration the binary lacks. That is the "newer schema" refusal.
- It refuses a dirty (partially applied) version.
- The exact sqlx 0.9 TLS feature for rustls on ring with native roots is `tls-rustls-ring-native-roots`.
- Migration discovery ignores files that do not match `<VERSION>_<DESCRIPTION>.sql`, so `migrations/.gitkeep` is safe.

Plan for issue #4:
- Store crate: `pool::connect(url, max_connections)` and `migrations::{MIGRATOR, migrate, check -> UpToDate | Pending | Newer | Dirty}`.
- The bin's role startup runs migrate-on-start or the check before the not-implemented stub.
- Postgres tests use `#[ignore = "requires PostgreSQL"]` plus `#[sqlx::test]`.
- A CI job with a matrix over postgres 16 and 18 runs `cargo nextest run --run-ignored only`.
