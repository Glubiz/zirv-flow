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

## v0.239.0 (2026-11-06)

### Fixes

- strip the carriage return, not the newline, from the sink's sequence number (#522)
- a stream's first sync flags its events no_triggers (#522)

### Documentation

- the first-sync exception for event triggers (#522)

### Chores

- drop a committed Python cache file and ignore __pycache__ (#522)
- the guide and verify match the first-sync, batch-lock and suppression-check fixes (#522)
- the first-sync tests run under the decision invariant (#522)
- bump the version to 0.239.0 (#522)

