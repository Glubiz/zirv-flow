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

## v0.245.0 (2026-11-06)

### Fixes

- the archive guard also covers active forwarding rules (#537)
- refuse archiving a destination a published flow uses (#537)

### Documentation

- correct the SMTP hard-bounce, failed-delivery and Retry-After sentences, document the destination guard (#537)

### Chores

- mark the new forwarding guard tests as needing PostgreSQL (#537)
- share the destination URI helper between the archive tests (#537)

### Other

- sim: the SMTP sink answers 5.1.1 for bounce recipients and verify checks the SMTP hard bounce (#537)

