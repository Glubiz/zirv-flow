# Spec: configuration and CLI roles (issue #3)

## Modules

`crates/zirv-flow/src/` has three files:

- `main.rs`: a thin composition root that maps errors to exit codes.
- `cli.rs`: the clap definitions.
- `config.rs`: loading, validation and redaction.

## Loading

- Sources: [figment](https://docs.rs/figment) with features `toml` and `env`,
  merged in this order: defaults, then the TOML file, then the env.
- Env mapping: `ZIRV_FLOW_<SECTION>__<KEY>`, case-insensitive. Variables with
  the prefix but no `__` (such as `ZIRV_FLOW_CONFIG`) are not config keys.
- Inputs: the loader takes `Option<&Path>` and an iterator of `(String, String)`
  env pairs. `main` passes `std::env::vars()`.
- Files: an explicit path that does not exist is an error. With no path, no file
  is read.
- Unknown keys: `#[serde(deny_unknown_fields)]` on every section.

## Keys

| Key | Type | Default | Rule |
|-----|------|---------|------|
| `server.bind` | socket address | `127.0.0.1:8080` | must parse |
| `database.url` | URL, secret | required | scheme `postgres` or `postgresql` |
| `database.max_connections` | u32 | 10 | >= 1 |
| `database.migrate_on_start` | bool | false | none |
| `worker.concurrency` | u32 | 4 | >= 1 |

## Errors

- Exit code 2. stderr shows `error: <key>: <reason> (from <file path | env VAR>)`.
- Messages never include a secret value.

## Redaction

- A `SecretUrl` newtype holds the database URL.
- Its `Debug` and `Serialize` replace any password with `***`.
- An unparsable value is reported only as "not a valid postgres URL".

## CLI

- Global flag: `--config <PATH>`, with env `ZIRV_FLOW_CONFIG`.
- Subcommands: `all` (the default), `serve`, `worker`, `scheduler`, `migrate`,
  `admin`, and `config check`.
- `config check` prints the effective config as TOML, redacted, and exits 0.
- Role and `migrate` commands load the config, print `error: the <name>
  command is not implemented yet` and exit 1.
- `admin` has no subcommands yet and prints the same message.

## Docs

README gets a short Configuration section: the file, the env form, the keys
table and `config check`.

## Version

0.3.0.
