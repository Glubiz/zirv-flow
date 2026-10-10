# Intent

## Problem

Issue #3. The `zirv-flow` binary parses no configuration and has no
subcommands. Every later M1 issue (database, HTTP server, queue) needs a
validated config and a role to run in.

## Desired outcome

- Configuration comes from an optional TOML file (`--config <path>`, or
  `ZIRV_FLOW_CONFIG`) plus `ZIRV_FLOW_<SECTION>__<KEY>` environment overrides,
  which win over the file. Built-in defaults apply where a key has one.
- The M1 keys from architecture section 17 are supported: `server.bind`,
  `database.url`, `database.max_connections`, `database.migrate_on_start` and
  `worker.concurrency`. The other keys in section 17 are added with the issue
  that first uses them.
- Validation errors name the key and where it came from (the file or the env
  variable) and exit with code 2. Unknown keys are errors.
- Secrets never appear in output. That covers the password in `database.url`
  in `config check` output, `Debug` output and error messages.
- The CLI has `serve`, `worker`, `scheduler`, `all` (the default with no
  subcommand), `migrate`, `admin` and `config check`. Role commands that are
  not implemented yet load and validate the config, then say so and exit 1.

## Constraints

- Config and CLI live in the bin crate (architecture 5.1). No new crate edges.
- Tests are inline. Loading takes its environment as an input, so tests never
  mutate the process environment.
- Version bump 0.2.0 to 0.3.0. No competing product is named anywhere.

## Open questions

None.

## Acceptance criteria

- [ ] An unknown key, in the file or the env, fails and names that key.
- [ ] An invalid value fails and names its key. A missing `database.url`
      fails and names `database.url`.
- [ ] An env override beats the file value.
- [ ] `config check` prints the effective config with the database password
      redacted. A value error on `database.url` does not echo the URL.
- [ ] `zirv-flow --help` lists every role and `config`.
- [ ] build, nextest, fmt, clippy, the crate-dependency gate and
      test-presence all pass. CI is green on the PR.
