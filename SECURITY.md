# Security policy

Threat model: [boundaries, controls, test evidence and accepted risks](docs/threat-model.md).

## Supported versions

Only the [latest release](https://github.com/Glubiz/zirv-flow/releases) is supported
for security updates.
The `/api/v1` HTTP API follows the [API contract policy](docs/api.md): from 1.0 it changes
only additively, and the policy names the surface that stays unfrozen.

## Reporting a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/Glubiz/zirv-flow/security/advisories/new)
via "Report a vulnerability" under the Security tab, or email josj@zirv.io.
Do not open public issues for vulnerabilities. Never include secrets, tokens,
or personal data of your contacts in reports.

Include:

- zirv-flow version (`zirv-flow --version`)
- Operating system
- Deployment method (Docker, binary, or built from source)
- Steps to reproduce
- Potential impact

Expect acknowledgement within 7 days.

## Dependency audits

Pull requests run all four `cargo deny --locked check` checks with cargo-deny
0.20.2 and `npm audit --omit=dev --audit-level=high` in `web/`. Rust policy and
advisory ignores live in `deny.toml`. The weekly dependency audit runs advisories
and the same npm command against `main` every Monday at 04:23 UTC, or through
`workflow_dispatch`. Failures are reported in the workflow log; no issue is opened.

An npm production finding must be fixed, preferably with a lockfile-only update.
If no fix is available, record the advisory, package/version and dependency path,
review date, exposure assessment and exit condition here before changing the gate.
There are currently no npm exceptions. Dev-only packages are outside the npm gate;
an audit service failure is not an exception and must fail the job.

## Known advisories

The ignore below was reviewed on **2026-10-27** and re-checked on **2026-10-31**
against `Cargo.lock`, the resolved dependency graph and the production call
sites. The affected version remains in the tree. Before the release candidate,
re-check it against the resolved tree and current RustSec advisory; remove it
once the dependency is gone or fixed. Reassess sooner whenever this cryptographic
use changes. Deferring the upstream upgrade retains the vulnerable transitive
code, so the restricted usage must continue to hold. RUSTSEC-2026-0387
(`crypto-bigint`) no longer applies: `openidconnect 4.1.0` resolves
`crypto-bigint 0.7.10`, and no 0.5 version remains.

### RUSTSEC-2023-0071 — rsa timing side channel

- Affected path: `zirv-flow-identity → openidconnect 4.1.0 → rsa 0.9.10`.
  `zirv-flow-core` also uses `rsa` to generate and read DKIM keys. Identity's mock
  provider and the connector verification tests use it only in test support.
- Exposure review: production ID-token verification uses the provider's public
  keys (`crates/zirv-flow-identity/src/oidc.rs`, `Provider::finish`). The affected
  RSA signing/decryption operations are not used in production: DKIM key
  generation is in `crates/zirv-flow-core/src/sending.rs`, and DKIM signing uses
  `ring::signature::RsaKeyPair` in
  `crates/zirv-flow-connectors/src/destinations/dkim.rs`. No elliptic-curve
  private-key operation is used by OIDC either.
- Reviewed: **2026-10-27**. Exit: upgrade every affected `rsa` path to a release
  RustSec marks fixed, or remove those paths, then delete the ignore. Reassess
  before adding RSA private-key signing or decryption through `rsa`.

## License exceptions

The project remains MIT-licensed. The permissive allowlist in `deny.toml` was
derived from offline Cargo metadata on **2026-10-27**. For dual-licensed crates,
the permissive alternative is used. The only non-permissive exceptions are
MPL-2.0, restricted to these exact crates and versions:

- `cssparser 0.38.0` via the HTML sanitizer `ammonia 4.2.4`.
- `dtoa-short 0.3.5` via `cssparser`.
- `option-ext 0.2.0` via `dirs-sys`.

Keep their license notices and make the MPL-covered source, including changes to
those files, available when distributing binaries. These exceptions do not
license other project files under MPL. Review the exception when upgrading any
of these crates; MPL is not on the general allowlist. See
[Mozilla's distribution guidance](https://www.mozilla.org/en-US/MPL/2.0/FAQ/#q8-i-want-to-distribute-outside-my-organization-executable-programs-or-libraries-that-i-have-compiled-from-someone-elses-unchanged-mpl-licensed-source-code-either-standalone-or-part-of-a-larger-work-what-do-i-have-to-do).

## Critical files

Changes here need extra scrutiny in review -- anything that runs in CI with
repository access. `scripts/security-scan.sh` reads this exact fenced list
(path before the first `:`) so the advisory CI scan and this document can
never drift; a trailing `*` means "this directory, any file directly or nested
inside it". Add entries as code that handles credentials, authentication, or
untrusted input lands.

```text
.github/workflows/*: CI/CD runs with a repository write token
.github/workflows/dependency-audit-weekly.yml: scheduled advisory checks against main
deny.toml: dependency sources, licenses, bans and advisory exceptions
scripts/*: run inside CI with repository access
```
