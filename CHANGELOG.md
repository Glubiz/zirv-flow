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

## v0.225.0 (2026-11-03)

### Features

- read the enrollments and lock the flows of a batch of steps at once (#397)

### Fixes

- record a failed wake's failure in its batch transaction (#397)
- read a tag's subject as a revision, never a path (#408)
- start a release's notes at the previous CD tag (#408)

### Performance

- run the claimed steps of a workspace in one transaction (#397)

### Documentation

- describe wake transactions, their lock order and invariants (#397)

### Chores

- leave the flow ordering to the locking statement (#397)
- bump the version to 0.225.0 (#397)
- model batch transactions in the engine harness (#397)
- drive the PostgreSQL simulation through wake transactions (#397)

