# Intent

## Problem

Issue #2. zirv-flow is one skeleton crate; architecture section 5 requires a
layered Cargo workspace whose crate edges and crate purity are enforced in CI.

## Desired outcome

A virtual workspace with `crates/zirv-flow` (bin), `zirv-flow-core`,
`zirv-flow-store`, `zirv-flow-api`, `zirv-flow-jobs`, wired along the 5.1
table; a CI check that fails on any workspace edge not in the 5.1 table and on
a pure crate gaining sqlx, an HTTP client or tokio networking; CI path filters
and agent context updated for `crates/`.

## Constraints

- Only M1 crates are created; later crates arrive with their milestone.
- The 5.1 table in `docs/architecture.md` stays the single source of truth.
- Repo conventions: edition 2024, inline tests, version bump, four gates green.

## Open questions

None.

## Acceptance criteria

- [ ] `cargo build`, `cargo nextest run --no-fail-fast`, fmt and clippy pass.
- [ ] The dependency check's self-test proves `api -> jobs` fails with a message
      naming the edge, and a pure crate depending on sqlx fails the purity check.
- [ ] CI runs both checks; version-bump, test-presence and security-scan path
      filters cover `crates/`.
- [ ] `zirv-flow --version`/`--help` behave as before.
