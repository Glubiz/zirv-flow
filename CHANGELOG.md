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

## v1.0.39 (2026-11-26)

### Fixes

- keep the 30 s startup wait; build both indexes before the drop in 0116
- keep the resume walk and the segment lookups bounded under stale statistics
- in-flight lookups stay bounded under stale statistics; refuse overload in 5 s

### Documentation

- record the stale in-flight statistics plan flip
- record the causes of the tier L stalls and the overload hang
- link the tier L capacity report from the README

### Chores

- regenerate the API client for the ingest 503 Retry-After
- separate the startup wait test
- simplify the bench stall fixes
- accept either scan of the flow state index in the rollup plan test
- pin the in-flight lookups and the pool acquire timeout under overload

### Other

- bench: correct the report's end-to-end percentiles and tighten the flow step criteria
- bench: tier L capacity and flow execution benchmark

