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

## v0.280.0 (2026-11-11)

### Fixes

- roll a failed drain batch back before it returns

### Performance

- apply a drain batch's event runs with one statement per write
- mark a batch's trait changes in one statement

### Documentation

- say that a paused flow skips the sends it had queued
- correct event disk sizing and warn about repeat sends after a restore
- describe the drain's batched event path

### Chores

- judge an event by its policy and schema in one place
- compare the outbox and the jobs of the two drain paths
- bump the workspace to 0.280.0

