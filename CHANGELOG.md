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

## v0.246.0 (2026-11-07)

### Chores

- split the PostgreSQL tests into two jobs per version; keep synchronous_commit on (#540)
- build the template program with the workspace features; say what a stale template does (#540)
- drop stray runs of spaces from the psql step and the reset test query (#540)
- give tag and dispatch runs a concurrency group of their own (#540)
- prepare the test template, split the OpenAPI gate, cancel superseded PR runs (#540)
- prepare a migrated template1 so tests skip per-test migrations (#540)

