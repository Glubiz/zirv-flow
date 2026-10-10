# Security policy

## Supported versions

Only the [latest release](https://github.com/Glubiz/zirv-flow/releases) is supported
for security updates.

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

## Critical files

Changes here need extra scrutiny in review -- anything that runs in CI with
repository access. `scripts/security-scan.sh` reads this exact fenced list
(path before the first `:`) so the advisory CI scan and this document can
never drift; a trailing `*` means "this directory, any file directly or nested
inside it". Add entries as code that handles credentials, authentication, or
untrusted input lands.

```text
.github/workflows/*: CI/CD runs with a repository write token
scripts/*: run inside CI with repository access
```
