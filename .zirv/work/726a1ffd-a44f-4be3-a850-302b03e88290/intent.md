# Intent

## Problem

Issue #6. zirv-flow has no accounts, no workspaces and no authentication. Every
later feature is workspace-scoped and needs an authenticated caller, whether a
person or an API key.

## Desired outcome

- Instance-level users are local accounts. Passwords are hashed with argon2id.
  The owner is an instance-wide role.
- Workspaces, with per-workspace memberships. The roles are `admin` and
  `member`; the owner implicitly administers every workspace.
- Server-side sessions use an HttpOnly, SameSite=Lax cookie. Every mutation
  authenticated by cookie must also carry a CSRF token.
- API keys are scoped to a workspace. They carry the scopes `ingest`, `read`,
  `write` and `admin`, and are stored only as hashes.
- An append-only audit log records sign-ins (including failed ones), sign-outs,
  user, workspace, membership and key events, and password resets. It stores no
  personal data values.
- A `zirv-flow admin create-owner` and `admin reset-password` CLI.
- Isolation: no caller ever reads or changes another workspace's data. Every
  workspace-scoped table carries `workspace_id`.

## Constraints

- Security-critical. Sign-in responses do not reveal whether an account exists,
  including through timing. Tokens and keys are never stored or logged in
  plaintext. Comparisons run in constant time.
- SQL lives in the store crate. Pure domain logic lives in core: roles, scopes,
  validation, password hashing and token generation. HTTP lives in api. The
  shared admin operations live in api, so the CLI and HTTP use the same code.
  The binary only composes.
- The first-run UI setup flow arrives with the UI shell (#9). In M1 the owner
  is created with the CLI.
- Version 0.6.0. No competing product is named anywhere.

## Open questions

None.

## Acceptance criteria

- [ ] After `zirv-flow admin create-owner`, the owner can sign in, create a
      workspace and create an API key through `/api/v1`. The key then
      authenticates a read on that workspace.
- [ ] Isolation tests:
  - a workspace admin and an API key from workspace A get 404 for every
    workspace-B resource;
  - a member cannot manage keys or members (403);
  - a schema test lists every table without `workspace_id` and compares it
    with an explicit instance-level allowlist.
- [ ] A cookie mutation without a valid CSRF token gets 403. The same mutation
      with the token, or authenticated by an API key, succeeds.
- [ ] A wrong password and an unknown email give the same 401 body. Repeated
      failures throttle the account.
- [ ] Audit rows are written for each listed event, and none contains a
      password, token or email.
- [ ] The OpenAPI document is regenerated. build, nextest, fmt, clippy,
      crate-deps and test-presence (run after committing) pass. CI is green,
      including PostgreSQL 16 and 18.
