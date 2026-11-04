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

## v0.232.0 (2026-11-04)

### Performance

- export pages of 10,000 rows and a one-statement snapshot begin (#397)

### Documentation

- state the export memory per stream as CSV text plus a multiple (#397)
- export page size and memory per stream; bump to 0.232.0 (#397)

### Chores

- scope the drain test's lock wait to its own guard (#397)
- tidy the snapshot test's query string (#397)

