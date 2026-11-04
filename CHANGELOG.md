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

## v0.231.0 (2026-11-04)

### Fixes

- link an explored event to its trace only when it holds a decision (#397)
- mark event timeline entries whose trace holds a decision (#397)

### Performance

- stop writing an event.accepted decision per accepted event (#397)

### Documentation

- say what the track latency measures since 0.231.0 (#397)

### Chores

- lay out the explorer has_decisions SQL literals as line-continued statements (#397)
- lay out the has_decisions SQL literals as line-continued statements (#397)
- name the WAL test for what it checks (#397)
- a trace that lost segment.entered fails complete (#397)
- share the whole-word scan and tidy the WAL test query (#397)
- bump to 0.231.0 (#397)
- historical-only registry rows and traces that start at the event (#397)

