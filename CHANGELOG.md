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

## v0.230.0 (2026-11-04)

### Fixes

- give up reading the inbox heads after a second (#397)
- claim at most one ingest.drain per batch (#397)
- lease capped claims from their update and hold batches from their return (#397)

### Performance

- end the capped claim's workspace probes at the due jobs and run it without JIT (#397)
- bound the queue, enrollment and inbox gauges (#397)
- confirm the drain's lease without a row lock (#397)
- claim the inbox drain ahead of due wakes (#397)
- claim 32 jobs per batch and hold them up to four seconds (#397)

### Chores

- read the depth plan's row count as a float, as PostgreSQL 18 reports it (#397)
- wait for the loading request before the retry mount (#397)
- queue the inbox drain at the default priority again (#397)
- run the held-snapshot horizon test alone (#397)
- bump the version to 0.228.0 (#397)

