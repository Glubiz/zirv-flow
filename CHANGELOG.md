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

## v0.237.0 (2026-11-06)

### Features

- provisioning, verification and the start, verify, sync-all and blast commands (#513)
- cap the CPU quotas, measure events per byte in status, seed via seed.py (#513)
- world, seed, traffic generators and traffic controls (#513)
- compose stack, fixtures and sim.sh for the simulated production environment (#513)

### Fixes

- a batch also locks the ledger rows of the people its records may merge away (#518)
- a re-evaluation claim leaves the mark of a person a writer holds (#518)
- lock a batch's new identities, ledger rows and people before its first record (#518)
- no segments before the first syncs by default, and a per-profile backfill timeout (#513)
- review round 1, provision defaults, verify checks tied to their run, reset order, status pause reason (#513)
- seed the company sources once, so down then up resumes the same world (#513)
- drop an unused loop variable (#513)
- drop an unused loop variable (#513)

### Documentation

- the batch lock takes the ledger rows of the people a record may merge away (#518)
- drop the OQ-2 citation from the HTTP-manifest checkpoint, which cites the rollup decision (#518)
- the lock order of a batch and the key-share lock of a claim (#518)
- the simulated production guide with measured start times, and verify and start fixes (#513)

### Chores

- keep the concurrent batch test's whole body inside the decision check (#518)
- version 0.237.0 (#518)
- share the directory setup, zf_sql, as_array and stamp through lib.sh (#513)

