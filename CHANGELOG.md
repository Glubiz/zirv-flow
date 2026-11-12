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

## v0.286.0 (2026-11-12)

### Fixes

- cap the stored import files and the open imports per workspace
- drop wholly expired event partitions before deleting rows

### Documentation

- keep the README within its line budget after the import keys
- state how far uploads in flight can pass the import quota

### Chores

- regenerate the API schema for the import quota responses
- keep version 0.286.0 after merging main
- bump version to 0.286.0

