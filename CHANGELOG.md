# Changelog

Generated from conventional-commit subjects (`feat`, `fix`, `perf`, `docs`,
plus `chore`/`refactor`/`test`/`style`/`ci`/`build`/`revert` folded into
Chores) by `scripts/generate-changelog.sh`. Never hand-edit this file --
regenerate a section instead:

    scripts/generate-changelog.sh --section --from <prev-tag> --to <new-tag>

`.github/workflows/cd.yaml` runs that command for every release and commits
the new top section here. A commit subject that does not match
`type(scope)!: summary` (most of the history before this file existed) is
listed under "Other" rather than dropped or guessed at. A `!` right after
the type/scope is the only breaking-change signal this script reads -- a
`BREAKING CHANGE:` footer in the commit body is not parsed. Full-rebuild
command: `scripts/generate-changelog.sh --full`.

## v0.278.0 (2026-11-10)

### Fixes

- rewrite a deleted person's audience pages with no savepoint
- hand on the rest of a settle job queued before the cap
- rewrite a deleted person's audience pages in bounded runs
- check the refresh poll's deadline before every try

### Performance

- keep wake and settle transactions within 64 subtransactions
- apply record-by-record units within 64 subtransactions
- wait for a busy refresh lock between statements

### Documentation

- refresh-lock polling and the subtransaction bound

### Chores

- start the waiting publish once the slow one holds the lock
- wrap the subtransaction tests in the decision invariant check
- bump 0.276.0

