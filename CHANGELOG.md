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

## v1.0.10 (2026-11-16)

### Fixes

- read the integer lookback window from the type minimum (#692, #695)
- keep keyless file keys stable and clamp the integer lookback (#690, #692, #695)
- injective keyless file keys, ISO dates under any DateStyle, an opt-in cursor lookback, a final code for null keys, row-value keyset pages, redirects to unresolvable hosts (#690-#695)

