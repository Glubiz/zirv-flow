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

## v1.0.0 (2026-11-12)

### Documentation

- point the checklist proofs at the latest CI run and fit the README status in its budget (#408)
- fill the 1.0.0 checklist, tick the M8 exit criteria, state 1.0 as the supported line (#408)
- a passed send gate writes no trace record since 0.269.0
- state only the measured index sizes for 0094 and drop the stale 'unreleased' for 0084
- review lock levels through 0095, add the 0094 events index rebuild

### Other

- release: set the workspace version to 1.0.0 (#408)

