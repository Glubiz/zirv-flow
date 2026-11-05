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

## v0.236.0 (2026-11-05)

### Features

- PostgreSQL settings A/B driver with I/O diagnostics for ingest and timers (#397)
- burst arm and inbox-to-applied latency in the ingest arm (#397)
- record per-arm pg_stat_statements and stop a jobs arm after 300 s without progress (#397)
- let M8_SEGMENTS_PHASES pick the segments phases (#397)
- instrument the decision-log A/B and report the append histogram (#397)
- explorer shape selection and a container mode for the explorer loadgen (#397)
- apply worker concurrency and audience size to the send bench, record the SMTP call time (#397)
- register the M8 decision-log A/B in container networking (#397)
- batch arm for the #76 tier-M run (#397)
- per-second latency timeline and run-start stamps taken before hostname (#397)
- worker concurrency knob and cheap drain poll for the timers bench (#397)
- run the M8 debugger watcher and clock check in a Linux container (#397)

### Fixes

- warn for an unset public URL with its own failure, not invalid_unsubscribe_url (#516)
- diagnose a non-https public URL and correct the worker help text (#516)
- build the re-armed wake jobs with a lateral record, since expanding (f()).* failed with a uuid error at 10 M rows (#397)
- the burst arm reads M8_SMOKE so a smoke stack gets the shortened burst (#397)
- end only the per-kind sampler session when its sampler stops, so the run's wait sampler survives jobs and timers (#397)
- draw the incremental phase's persons from a fresh seed so earlier loads cannot pre-fill the segment (#397)
- let M6_ALLOW_HOST_CARGO skip the host cargo wait for capped M8 runs (#397)
- verify a reused sending domain by SQL again when the product's re-check reset it (#397)
- make the incremental segments phase produce event-caused entries and fail when it does not (#397)
- stop every sampler on every exit path, refuse leftovers, sample docker stats by name, follow an image switch (#397)
- let loadgen keep one idle connection per in-flight request (#397)
- test the A/B binaries for size, not the exec bit (#397)
- count 503 explorer_busy as an expected explorer outcome and let reruns pick scenarios (#397)
- M8 bench instrumentation, quiet start, drain poll and drift-based debugger gate (#397)
- keep the narrowest of ten clock brackets in the debugger check (#397)
- run explorer and debugger scripts on Windows hosts (#397)
- keep container paths intact under Git Bash (#397)

### Documentation

- correct the burst drain start, capped-job concurrency, timer target scope and debugger delivered share (#397)
- the M8 validation report, sizing conclusions and README link (#397)
- describe the sampler, explorer, decisions, send and incremental changes (#397)

### Chores

- name the public URL check for what it tests (#516)

### Other

- bench(m8): commit the M8 validation data files (#397)

