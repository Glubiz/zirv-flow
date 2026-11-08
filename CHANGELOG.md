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

## v0.267.0 (2026-11-08)

### Performance

- key drain lanes by the resolved person (investigation L)

### Documentation

- append_inbox now makes a person lookup before its insert

### Chores

- let the busy-explorer test's hold wait for the pool's one connection
- make k a known person before the empty-check race so both rows share its lane
- share the lane tests' person fixtures, flatten lane_key, unbreak the lookup SQL

