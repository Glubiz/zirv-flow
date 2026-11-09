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

## v0.269.0 (2026-11-09)

### Fixes

- run the enroll flow lock statement as written, not wrapped in a second SELECT

### Performance

- drop send.gate passed rows and sync.record applied rows that a trait.change covers
- enroll takes the flow lock shared and one lock per person
- one duplicate lookup per batch, change readers checked once per batch, one-statement feedback dedupe
- x5 database profile with 4GB shared_buffers, jit off and a 30 minute checkpoint

### Chores

- rustfmt the sync batch test
- break the long space runs in the new SQL strings and keep the enroll lock statements static
- update jobs and e2e expectations for the dropped decision rows
- regenerate the send-path trace fixtures without passed gate rows

