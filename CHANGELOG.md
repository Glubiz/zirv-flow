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

## v1.0.20 (2026-11-18)

### Fixes

- take the key repair lock only for an empty key file, and name the upload deadline test for what it proves (#751, #753)
- fall back only on a healthcheck connect error, lock the key repair, and test the real upload deadline (#751, #752, #753)
- bind, readiness, config, master key, healthcheck and listener review findings (#748-#753)

### Chores

- flatten the empty-key repair in generate-master-key

