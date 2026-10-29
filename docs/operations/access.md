# Accounts and sign-in

## Accounts, workspaces and authentication

People sign in with a local account (e-mail and password, hashed with argon2id).
One account is the instance **owner**; the owner administers every workspace.
Other accounts are members or admins of individual workspaces. Create the owner
once, from the command line or through the first-run setup below:

```sh
printf '%s
' "$OWNER_PASSWORD" | zirv-flow admin create-owner --email owner@example.com
printf '%s
' "$NEW_PASSWORD"   | zirv-flow admin reset-password --email someone@example.com
```

Both commands read the password from one line of standard input (never from an
argument), connect, run the same startup schema check as `serve`, and exit 1 on
any error. `reset-password` also clears a lockout and ends the user's sessions.
Passwords are 12 to 1024 characters.

- **First-run setup.** While no owner exists, `serve` mints a one-time setup
  token and logs it once, as a `warn` line: `no owner exists; finish setup at
  <public_url or http://<bind>>/setup with setup token <token>, or run
  zirv-flow admin create-owner`. `GET /api/v1/setup` answers
  `{"required": true}` until an owner exists. `POST /api/v1/setup` with
  `{token, email, password}` creates the owner and signs them in exactly like
  `POST /api/v1/session` (cookie, user and `csrf_token`); a missing or wrong
  token is 403, an existing owner 409. The token lives only in that process's
  memory (as a SHA-256) and ends when an owner exists by any path or the process
  restarts, which mints a new one. With several `serve` processes each has its
  own token and any one works against its own process; on a multi-instance
  deployment use `admin create-owner`.
- **Jobs administration.** Owner session only; API keys get 403.
  `GET /api/v1/admin/jobs` shows ready, running and dead counts per job kind and
  the age of the oldest runnable job. `GET /api/v1/admin/jobs/dead?limit=&cursor=`
  lists dead jobs newest first (payloads are never shown), with their
  `trace_id`; `POST /api/v1/admin/jobs/{id}/retry` puts a dead job back in the
  queue under its original trace, and is audited.
- **Sessions.** `POST /api/v1/session` with `{email, password}` returns the user
  and a `csrf_token` and sets the `zf_session` cookie (HttpOnly, SameSite=Lax,
  Secure unless `server.cookie_secure` is false). Sessions expire after 12 hours
  idle or 7 days in total. `GET /api/v1/session` shows the current user and CSRF
  token; `DELETE /api/v1/session` signs out.
- **OIDC sign-in.** With `auth.oidc` configured, `GET /api/v1/session/oidc/start`
  redirects (303) to the identity provider using the authorization-code flow
  with PKCE (S256); the provider sends the browser back to
  `{server.public_url}/api/v1/session/oidc/callback`, which starts the same
  session as a password sign-in and redirects to `server.public_url`. The login
  in progress is stored server-side, single-use and valid for 10 minutes, and is
  tied to the browser by the HttpOnly `zf_oidc_state` cookie. An identity is
  matched by issuer and subject; failing that, by a **verified** e-mail address
  to an existing user, which is then linked. With `auto_create_users`, an
  unknown user with a verified address is created without a password and
  without workspace access: provider roles and groups are not mapped, so an
  admin grants memberships. Any failure is one generic 401 (the reason goes to
  the audit log only). Discovery is lazy and cached for 15 minutes, so the
  server starts while the provider is down; `start` then answers 503. With
  `auth.local.enabled = false`, `POST /api/v1/session` is 404;
  `zirv-flow admin reset-password` still works.
- **CSRF.** A request authenticated by the cookie that uses POST, PUT, PATCH or
  DELETE must send the token in an `x-csrf-token` header, or it gets 403. Sign-in
  needs none.
- **API keys.** A workspace admin creates a key with scopes (`ingest`, `read`,
  `write`, `publish`, `admin`, `pii`); the response shows the `zfk_...` key once, and
  only a hash is stored. Send it as `Authorization: Bearer <key>`. A key works
  only in its own workspace and never on instance-level endpoints (`/users`,
  creating workspaces, `/session`). `read` allows GETs, `write` the editor-level
  drafts, `publish` the publisher actions and `admin` members, keys and the
  rest of the admin routes; scopes do not imply one another, so a `write` key
  cannot publish and a `publish` key cannot edit. `pii` shows personal values
  unmasked and reads nothing alone: a key needs `read` as well.
- **Roles.** A workspace membership is `admin`, `publisher`, `editor` or
  `viewer`; the instance owner is a flag on the user, not a role. A viewer
  reads. An editor also changes drafts (segments, flows, templates, snippets,
  themes, lists, contacts), previews, test mode and test sends. A publisher
  also publishes, pauses, resumes and archives flows, retries, skips and exits
  enrollments, starts a flow for an audience, publishes templates, snippets and
  themes, and runs imports. An admin also manages sources, destinations,
  credentials, keys, members, retention, privacy, consent policy, event
  schemas, suppressions, forwarding, sending domains and webhooks. A refused
  request is a 403 that names the role the route needs. Older `member` rows
  became `editor`. Removing or demoting a workspace's last admin is refused
  with a 409 (`last_admin`) unless the caller is the instance owner. Where a
  section below says "any member", read editor or above; "admin" on a publish
  route reads publisher or above.
- **Personal values.** The instance owner, a workspace admin and an `admin` key see
  identifiers, trait values, event properties and consent evidence unmasked. A
  publisher, editor or viewer sees them masked unless the admin sets `view_pii` on
  their membership (`PUT .../members/{user_id}` with `"view_pii": true`; leaving it
  out keeps the stored value; `GET .../members` shows it). A key sees them with the
  `pii` scope. The grant is also what an editor needs to preview, test-send or test a
  flow as a real person. Migration 0078 starts every membership at false.
- **Upgrading to roles.** Migration 0073 gives every existing `admin` key the
  `publish` scope, so deployed admin keys keep publishing. A `write` key that
  runs imports (`POST .../imports/{id}/run`) needs `publish` added by hand.
  Former members are now editors, who may also create assistant drafts.
- **Sign-in throttling.** Five failures in a row lock the account for
  `min(2^(failures-5) seconds, 15 minutes)`. An unknown e-mail, a wrong password
  and a locked account give the same 401.
- **Isolation.** A caller with no access to a workspace gets 404 for it, never
  403. Workspace members of any role can read the workspace and its member list;
  admins and the owner also manage members and keys.
- **Audit log.** Sign-ins (including failures), sign-outs, user, workspace,
  membership and key events and password resets are appended to `audit_log`.
  Rows hold ids, the request id and a small summary, never e-mails, passwords,
  tokens or key secrets. The log is append-only: the code offers no update or
  delete. Ids are not commit-ordered, so an entry committed during a page walk
  or an export may be missing from it; the `rows` count in `audit.exported` is
  the rows streamed. Only the owning role may execute `audit_purge`; an
  operator who runs the app under a separate role grants it EXECUTE.

| Endpoint | Who |
|----------|-----|
| `POST/GET/DELETE /api/v1/session` | anyone / signed in / signed in |
| `GET /api/v1/session/oidc/start`, `GET .../oidc/callback` | anyone; 404 unless `auth.oidc` is configured |
| `GET/POST /api/v1/users` | owner |
| `GET /api/v1/workspaces`, `POST /api/v1/workspaces` | signed in / owner |
| `GET /api/v1/workspaces/{ws}`, `.../members` | members, admins, owner, `read` keys |
| `PUT/DELETE /api/v1/workspaces/{ws}/members/{user_id}` | workspace admins, owner, `admin` keys |
| `GET/POST /api/v1/workspaces/{ws}/api-keys`, `DELETE .../api-keys/{id}` | workspace admins, owner, `admin` keys |
| `/api/v1/workspaces/{ws}/credentials`, `.../credential-kinds` (see Workspace credentials) | workspace admins, owner, `admin` keys |

## Sign in with OIDC

Use an OpenID Connect provider that publishes discovery. Register a confidential
client for the authorization-code flow with PKCE S256 and the exact redirect URI
`<server.public_url>/api/v1/session/oidc/callback`. Give the client the scopes
`openid`, `email` and `profile`, and ensure users have verified email addresses.

For example, the deployment's TOML can contain:

```toml
[server]
public_url = "https://flow.example.com"

[auth.oidc]
issuer_url = "https://identity.example.com"
client_id = "zirv-flow"
auto_create_users = false
```

Set `ZIRV_FLOW_AUTH__OIDC__CLIENT_SECRET` to the client's secret. Create the user
first (`zirv-flow admin create-owner` for the first owner, or
`POST /api/v1/users` for another account). Open
`/api/v1/session/oidc/start`, sign in, and check `GET /api/v1/session` for the
resulting session. A verified email links an existing local user on first login.
With `auto_create_users = true`, an unknown verified user is created but has no
workspace membership until an admin grants it. Roles and groups are not imported.

For plain HTTP development on localhost only, set `server.cookie_secure = false`.
Once SSO works, `auth.local.enabled = false` disables password login; the password
reset CLI remains available. Protect the master key and identity-provider client
secret as described in [Secrets](secrets.md).
