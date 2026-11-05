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

## v0.234.0 (2026-11-05)

### Fixes

- keep a batch's heartbeats beating until the batch ends (#397)
- step each wake of a batch at its own start on the batch's clock (#397)

### Performance

- index enrollments by person for the person cascade (#510)
- make the settle of a wait step a HOT update (#397)
- read a wake batch's clock, append its decisions and complete its wakes once (#397)
- complete a batch of jobs in one fenced delete and keep a failed flush's decisions (#397)

### Documentation

- claim a HOT settle only on a page with room (#397)
- point the upgrade link at the lock review through 0090 (#397)
- say what a lease lost at a wake batch's completion does (#397)

### Chores

- leave the round-trip bound of a full batch a margin (#397)
- keep the round-trip test's body to the decision invariant wrapper (#397)
- bump to 0.234.0 (#397)

