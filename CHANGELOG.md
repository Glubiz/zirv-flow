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

## v1.0.21 (2026-11-18)

### Fixes

- keep a re-uploaded Show more image in place, and drop the Show more rows at once after a re-drive (#756, #761)
- assets, API keys, assistant, consent, forwarding, destinations and events web review findings (#756-#765)

### Chores

- page from the reloaded first page, keep the rows on a failed re-drive, and prove the late answer was pending (#761)

