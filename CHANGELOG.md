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

## v0.244.0 (2026-11-06)

### Fixes

- keep the replay comment true of every record reject
- keep issues on the segment read only; read dropped-trait rejects as records (#535, #534)
- refuse archiving a trait a live segment reads (#535)
- a merge never stamps an exit before the row's entry (#536)
- leave a reject naming the traits an identify dropped (#534)

### Chores

- merge the ingest imports and wrap the merge close statement and comment (#536)
- keep the dropped-traits test body inside the decision wrapper (#534)

