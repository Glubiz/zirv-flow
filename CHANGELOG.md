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

## v1.0.37 (2026-11-24)

### Fixes

- lock the erased group's own records before its people (#554)
- lock other records before the people, and rename erased tombstone keys (#554)
- an erasure reaches every record that lists the value (#554)

### Documentation

- reflow the erase lock-order comment (#554)

### Chores

- drop the unused held argument of lock_matching_records (#554)
- read the erased pairs from one CTE (#554)

