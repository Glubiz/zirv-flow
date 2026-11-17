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

## v1.0.17 (2026-11-17)

### Fixes

- review round 2 finding for the flow probe guard (#721)
- review round 1 findings for the warm-up audit and the probe guard (#721, #722)
- flow signing, validation cost, probe guard, warm-up and resume review findings (#717-#723)

### Chores

- ignore the chrono and chrono-tz unmaintained advisories (#830)
- one extend per fact map and the lifecycle doc comment back in place (#719, #723)

