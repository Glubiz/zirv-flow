# Contributing to Zirv Flow

Follow the [Code of Conduct](CODE_OF_CONDUCT.md). Report vulnerabilities through
our [security policy](SECURITY.md).

## Reporting bugs and requesting features

Use the [GitHub issue forms](https://github.com/Glubiz/zirv-flow/issues/new/choose).
Include enough detail to reproduce a bug or explain the problem a feature solves.
Never include secrets, tokens, or personal data of your contacts in reports.

## Development setup

Install the stable Rust toolchain, then run from your checkout:

```sh
cargo install cargo-nextest
cargo build
```

For what the project is and where it is going, see the [README](README.md).

## Making changes

Branch from `main`; never commit directly to `main`. Keep diffs focused and mirror
existing naming, structure, and style. Tests stay inline in `#[cfg(test)] mod tests`;
`tests/fixtures/` is data only.

Follow the enforced crate, feature-module, registry and frontend boundaries in
[architecture section 5](docs/architecture.md#5-module-and-crate-layout). Put new
code behind the owning module's public surface; never loosen a boundary check to
make a change pass.

Size verification to the change using [CLAUDE.md](CLAUDE.md#build-and-verify-by-tier):

- Docs or comments: run `cargo fmt -- --check` if Rust was touched; otherwise no checks.
- Code: run `cargo build`, `cargo nextest run <filter>` for touched modules, and
  `cargo clippy --all-targets -- -D warnings`.
- Before opening or updating a PR: run all four gates below once.

## Before opening a PR

```sh
cargo build
cargo nextest run --no-fail-fast
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

Bump the version in `Cargo.toml` above its base version on `main` whenever the
diff touches `src/`, `Cargo.toml`, `Cargo.lock`, or `build.rs` -- CD's release
step is idempotent on an already-published version, so a docs/CI-only PR needs
no bump. Run `cargo build` to update `Cargo.lock` and include it.

Commit subjects use `type(scope): summary`, with types such as `feat`, `fix`,
`docs`, and `chore`; for example, `fix(segments): handle empty audiences`.
Do not include `Co-Authored-By` or `Generated with Claude Code` lines in commits.

## Pull requests

Open a PR targeting `main` with one logical change and explain what changed and
why. CI runs on pull requests to `main` and is the merge gate.
