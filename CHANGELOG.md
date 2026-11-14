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

## v1.0.1 (2026-11-14)

### Fixes

- update ammonia to 4.2.5 for RUSTSEC-2026-0533
- harden summary, SPF, masking, redaction, webhook URL and cron validation (#611-#617)

### Documentation

- tick the M8 milestone close in the 1.0.0 checklist
- correct the 5x page's PostgreSQL memory settings and rate wording
- publish the 1.0.0 5x simulated benchmark and scaling guidance
- record the 1.0.0 post-publish proofs and tick M8 criterion 9 (#408)

### Chores

- fold the dotted-key descent into one loop and match the six cron fields
- rewrap the PostgreSQL matrix comment
- run the PostgreSQL tests in four partitions per version

