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

## v0.273.0 (2026-11-09)

### Features

- claim segment.reevaluate ahead of other jobs

### Fixes

- a reevaluate competes in band order in a capped claim
- a batch claim takes at most one segment.reevaluate
- a full segment refresh takes its people's KEY SHARE in id order

### Documentation

- reevaluate priority band and the full refresh's lock order

### Chores

- a capped claim keeps a place of the cap for the reevaluate
- format the lock-order test and drop a stray ORDER BY from plan_query
- wrap the refresh lock-order test in decision_testing::checked

