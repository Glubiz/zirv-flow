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

## v1.0.18 (2026-11-17)

### Fixes

- guard the export snapshot in the store and answer 422 on rotating an equal-header hook
- review round 1 findings for the export, the covering index and the header check (#737, #739, #740)
- contacts, webhooks, preferences, export, stats, debugger and stream review findings (#734-#740, #754, #755)

### Chores

- restore a line continuation in the preference page markup

