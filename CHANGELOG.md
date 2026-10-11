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

## v0.16.0 (2026-10-11)

### Features

- operator master key and per-purpose HMAC keys
- web UI shell with setup, sign-in, workspaces, members, API keys, jobs, traces and health
- embed and serve the web UI with a strict CSP
- optional OTLP trace export
- first-run setup and jobs-admin API
- metrics and health listener for worker-only and scheduler-only processes
- container image, compose quickstart and healthcheck
- trace ids, decision log, JSON logs and queue metrics
- durable job queue and scheduler
- OIDC sign-in with Keycloak as the reference provider
- workspaces, local accounts, sessions and API keys
- axum server with health, readiness, metrics and OpenAPI
- PostgreSQL pool and embedded migrations
- load TOML and env configuration and add the CLI roles
- split into layered crates with enforced dependency rules

### Fixes

- reach the master key through the store and fix backup wording
- lock the build caches so the two image architectures cannot race
- keep focus in the key secret dialog, reset dialogs on workspace switch, sign out on 401
- report the asset's length on HEAD
- show one active nav item without a workspace and print empty values as a dash
- give every OpenAPI operation a unique id and drop the client-side dedup
- redact OTLP endpoint credentials, reject query endpoints, accept future traceparent versions
- refuse an overflowing dead-letter cursor and restore split SQL lines
- publish the quickstart on localhost and scope the smoke job's token
- keep partition upkeep going and zero gauges on step-down
- open a warm pool's minimum connections at start
- keep heartbeat connections warm and give a beat one period to acquire
- isolate worker pools from HTTP and handlers, one shutdown deadline, stronger fence tests
- resolve concurrent OIDC first sign-ins and log provider failures
- keep the hashing permit until the blocking work ends
- close sign-in timing leak, bound argon2, use DB clock for locks
- report the schema refusal even when the unlock fails
- refuse edited migrations and keep the migration lock out of the pool
- mark the CI helper scripts executable

### Documentation

- decide OQ-3 (no unmerge in v1) and OQ-20 (HTTP API only)
- record that the image is built from the release tag
- plan native contacts and a Kafka source
- render the native context files from LF sources
- keep the module map within the context budget
- restore line continuations in the Keycloak commands
- plan sender authentication in M6 and multiple themes in M7
- make module boundaries a first-class, enforced design rule
- add event logging, traceability and LLM assistance to the design
- link theme and snippet versions to the assets they reference
- close review gaps in the template builder design
- add the multi-channel template builder to the architecture
- add architecture and milestone roadmap

### Chores

- simplify the master key change
- release only the Linux binary
- release 0.15.1 so the image is built with the locked build caches
- run the owner's API flow on the compose stack and refuse startup through run
- an owner creates a workspace through the UI
- drop an unused test helper
- publish on every merge to main
- build the subscriber once and drop an unused reqwest feature
- cover readiness through the real role wiring; list ops in the config row
- reuse the readiness alias and restore a misplaced doc comment
- keep the step-down test's backend kill in its own database
- date partition tests off the clock and test step-down
- ignore RUSTSEC-2023-0071 for verify-only RSA use
- give the pool-saturation test a lease a slow runner cannot outlast
- tighten the queue and scheduler code
- tighten the OIDC sign-in code
- make the lockout acceptance test independent of runner speed
- share path-id parsing and return impl IntoResponse
- unit-test each new API module
- share one helper across the crate-dependency checks
- set up the repository like zirv-cli

### Other

- Initial commit

