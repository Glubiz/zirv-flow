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

## v1.0.9 (2026-11-16)

### Fixes

- end a file of unknown size only under a strong ETag, keep a size named on any window (#688)
- end a file of unknown size only under If-Range, test the refusal reset on a move (#688, #689)
- share outbound clients, skip the QUIT reply, cap retries first, fail loud on bad links, encode token credentials, end files at their known size, stop offset-refusal loops (#683-#689)

