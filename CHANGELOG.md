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

## v0.281.0 (2026-11-11)

### Fixes

- rebuild 0095's index when the one under its name is invalid or different

### Performance

- mark records seen only in a run that sweeps
- lock a batch with no merging owner in one pass
- read only the ledger records that hold traits

### Documentation

- keep round measurements out of the ledger read comments
- the ledger's traits index, the one-pass lock, sweep marks and 0095's upgrade cost

### Chores

- a key whose only record empties leaves the person and its sources
- name the batch's record lookup seen_ids
- bump 0.281.0

