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

## v1.0.26 (2026-11-20)

### Fixes

- stop a privacy refresh loop after inputs return; keep an exhausted event name out of other retries
- keep a Show more page over a shallower poll, reset the page depth per input, scope event-type retries to the workspace
- refresh every loaded privacy page on a poll; retry each event name on its own backoff
- privacy, segments and sending settings web review findings (#784-#793)

### Chores

- the segment explain panel reads segments and lists through the catalog hooks

