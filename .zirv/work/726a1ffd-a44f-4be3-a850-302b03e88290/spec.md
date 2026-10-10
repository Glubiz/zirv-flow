# Spec: workspaces, accounts, sessions, API keys (issue #6)

## Core (`zirv-flow-core`, pure)

- **Ids.** UUID newtypes: `UserId`, `WorkspaceId`, `ApiKeyId`.
- **`Role`.** `Admin`, `Member`. The owner is a user flag, not a membership role.
- **`Scope`.** `Ingest`, `Read`, `Write`, `Admin`. Each parses from and displays as a lower-case string.
- **`Email`.** Trimmed and lower-cased, with a basic syntax check: one `@`, a non-empty local part and domain, at most 254 characters.
- **`WorkspaceName`.** 1 to 100 characters after trimming.
- **`Slug`.** `[a-z0-9-]`, 1 to 63 characters, no leading or trailing `-`. Derived from the name, or given explicitly.
- **`password` module.**
  - `hash(&str) -> String` uses argon2id with the `argon2` crate's default (OWASP) parameters and returns a PHC string.
  - `verify(&str, &str) -> bool`.
  - Policy: 12 to 1024 characters.
  - `dummy_verify()` runs one verification against a fixed hash, so sign-in for an unknown email takes the same time as for a real one.
- **`token` module.**
  - `random_token(bytes) -> String`: base64url with no padding, from `OsRng`.
  - `sha256(&str) -> [u8; 32]`.
  - Constant-time comparison through `subtle`.

## Store (`zirv-flow-store`)

### Migrations

Migrations use a 4-digit sequence, starting with `0001_accounts.sql`.

| Table | Columns |
|-------|---------|
| `users` | `id uuid pk`, `email text not null`, `password_hash text not null`, `is_owner bool not null default false`, `failed_sign_ins int not null default 0`, `locked_until timestamptz`, `created_at timestamptz not null default now()` |
| `workspaces` | `id uuid pk`, `name text not null`, `slug text not null unique`, `created_at` |
| `memberships` | `workspace_id uuid not null references workspaces`, `user_id uuid not null references users`, `role text not null check (role in ('admin','member'))`, `created_at`. Primary key `(workspace_id, user_id)`. |
| `sessions` | `token_hash bytea pk`, `user_id uuid not null references users on delete cascade`, `csrf_hash bytea not null`, `created_at`, `last_seen_at`, `expires_at timestamptz not null` |
| `api_keys` | `id uuid pk`, `workspace_id uuid not null references workspaces`, `name text not null`, `secret_hash bytea not null`, `scopes text[] not null`, `created_by uuid references users`, `created_at`, `last_used_at`, `revoked_at` |
| `audit_log` | `id bigserial pk`, `workspace_id uuid` (null for instance events), `actor_kind text not null` (`user`, `api_key`, `system`, `anonymous`), `actor_id uuid`, `action text not null`, `object_kind text`, `object_id uuid`, `request_id text`, `summary jsonb not null default '{}'`, `occurred_at timestamptz not null default now()` |

- `users` has a unique index on `lower(email)`.
- The repository functions for these tables live in feature modules (`accounts`, `workspaces`, `sessions`, `api_keys`, `audit`).
- Queries are checked at runtime (`sqlx::query_as` with `FromRow`). The offline query cache is deferred; record that in the PR.
- The audit log is append-only. It has no update or delete function.

## API (`zirv-flow-api`)

### Authentication

- **Session cookie.** `zf_session`, an opaque random token of 32 bytes.
  - HttpOnly, SameSite=Lax, Path=/. Secure unless the new key `server.cookie_secure` is false (default true).
  - Idle timeout 12 h and absolute timeout 7 days. `last_seen_at` is refreshed at most once a minute.
- **CSRF.**
  - A random token is issued at sign-in and stored hashed.
  - It is returned in the sign-in body and by `GET /api/v1/session`.
  - A request authenticated by cookie with method POST, PUT, PATCH or DELETE must carry a matching `x-csrf-token` header, or it gets 403 problem+json.
  - Sign-in itself needs no token.
- **API keys.**
  - The key is `zfk_<api-key-uuid-simple>_<secret>`, with a 32-byte secret.
  - The caller sends `Authorization: Bearer <key>`.
  - Lookup is by id. The SHA-256 hash of the secret is compared in constant time.
  - A revoked key gets 401.
  - `last_used_at` is updated at most once a minute.
  - An API key never reaches instance-level endpoints (`/users`, `POST /workspaces`). Within its own workspace, the scopes allow:

    | Scope | Allows |
    |-------|--------|
    | `read` | GETs |
    | `write` | mutations, except members and keys |
    | `admin` | members and keys |
    | `ingest` | only the ingest endpoints (none yet) |
- **Sign-in throttling.**
  - Five consecutive failures set `locked_until = now() + min(2^(failures-5) s, 15 min)`.
  - While an account is locked, sign-in returns the same 401 as a wrong password, without verifying the password.
  - A successful sign-in resets the counter.
  - An unknown email runs `dummy_verify` and returns the same 401.

### Authorization

| Caller | May |
|--------|-----|
| Owner | Anything |
| Workspace admin | Read the workspace; manage its members and keys |
| Workspace member | Read the workspace and its member list |
| Anyone else | 404 for that workspace's resources. Never 403, so existence does not leak. |

### Endpoints

| Endpoint | Who | Notes |
|----------|-----|-------|
| `POST /api/v1/session` | anyone | Body `{email, password}`. Returns 200 `{user, csrf_token}` and sets the cookie. Failure is 401 problem `invalid email or password`. |
| `GET /api/v1/session` | signed in | Returns the current user and the CSRF token. |
| `DELETE /api/v1/session` | signed in | Sign out. |
| `POST /api/v1/users` | owner | Body `{email, password}`. |
| `GET /api/v1/users` | owner | |
| `GET /api/v1/workspaces` | signed in | Workspaces the caller can see. |
| `POST /api/v1/workspaces` | owner | Body `{name, slug?}`. A slug conflict is 409. |
| `GET /api/v1/workspaces/{ws}` | see Authorization | |
| `GET /api/v1/workspaces/{ws}/members` | see Authorization | |
| `PUT /api/v1/workspaces/{ws}/members/{user_id}` | admin | Body `{role}`. |
| `DELETE /api/v1/workspaces/{ws}/members/{user_id}` | admin | |
| `GET /api/v1/workspaces/{ws}/api-keys` | admin | Never returns secrets. |
| `POST /api/v1/workspaces/{ws}/api-keys` | admin | Body `{name, scopes}`. Returns the plaintext key once, with `201`. |
| `DELETE /api/v1/workspaces/{ws}/api-keys/{id}` | admin | Revokes the key. |

- `{ws}` is the workspace UUID.
- Errors use problem+json with 400, 401, 403, 404, 409 and 422 as appropriate.
- Validation failures are 422 and name the field, never echoing a password.
- Every endpoint carries utoipa annotations. Regenerate `openapi.json`.

### Admin operations

A shared module, `zirv_flow_api::admin`, holds `create_owner`, `reset_password` and the other operations. The HTTP handlers and the CLI both call it, and it writes the audit rows.

## Audit

| Action | Recorded when |
|--------|---------------|
| `session.signed_in` | a sign-in succeeds |
| `session.sign_in_failed` | a sign-in fails. The actor is the user if the email exists, otherwise `anonymous` with no id. The summary is `{reason: "invalid_credentials" \| "locked"}`. |
| `session.signed_out` | a user signs out |
| `user.created` | a user is created |
| `user.password_reset` | a password is reset |
| `workspace.created` | a workspace is created |
| `membership.set` | a membership is added or changed |
| `membership.removed` | a membership is removed |
| `api_key.created` | a key is created |
| `api_key.revoked` | a key is revoked |

Each row carries the request id when there is one. Summaries never contain emails, passwords, tokens or key secrets.

## Binary

- `zirv-flow admin create-owner --email <e>` reads the password from stdin (one line). It fails if an owner already exists.
- `zirv-flow admin reset-password --email <e>` also reads stdin.
- Both connect and run the startup gate (migrating when `migrate_on_start` is set). Errors exit with 1.
- The pool reaches `AppState` as now.

## Tests

### Core (unit)

- validation;
- the password policy and hash/verify;
- token format;
- scope parsing.

### PostgreSQL tests (`#[ignore]` + `#[sqlx::test]`)

- the store repositories;
- the end-to-end acceptance flow, through the router with `oneshot`: owner signs in, creates a workspace and a key, and the key reads;
- CSRF enforcement;
- isolation across two workspaces, both for a session admin and for a key;
- the member gets 403;
- the throttle and lockout;
- unknown email and wrong password give equal bodies;
- revoked keys;
- session expiry;
- audit rows, checking that no row contains the email or password;
- the schema test, which compares tables without `workspace_id` with the allowlist `{users, workspaces, sessions, audit_log, _sqlx_migrations}`.

### Binary

- `admin create-owner` with stdin.

Every changed `.rs` file needs its own `#[cfg(test)]` module, because the test-presence gate checks this on committed files.

## Docs

- README: auth overview, the admin CLI, and the `server.cookie_secure` key.
- `.zirv/context/common.md` module map, then regenerate CLAUDE.md and AGENTS.md.

## Version

0.6.0.
