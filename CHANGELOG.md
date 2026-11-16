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

## v1.0.11 (2026-11-16)

### Fixes

- give back an orphan's unsent cap slot, stamp each retention window on its own, keep the forwarding order at an empty bucket (#697, #698, #702)
- hold sends while the bucket is empty, give back unsent cap slots, stamp only real retention runs, skip unmarked paused retries, suppress bounces for erased people, stop event forwarding at an empty bucket (#696-#702)

