# Secrets and the master key

This guide covers what the master key protects, what losing it costs, how to notice, and how to
rotate it. The design is in `docs/architecture.md` section 16.2; the rotation steps are in the
README under Secrets.

## What the key protects

The master key wraps the random data key of each sealed value. Two tables hold sealed values, and
nothing else in the database is sealed:

- `encrypted_secrets`: one row per secret, of one of the purposes below.
- `deployment_keys`: the HMAC root, one row.

Everything else in the database is stored in the clear, and a database dump without the master key
shows ciphertext, wrapped data keys and the id of the wrapping key for these two tables only.

### Secret purposes

Every row of `encrypted_secrets` has one of these purposes.

| Purpose | Holds |
| --- | --- |
| `workspace_credential` | The secret fields of a workspace credential. |
| `connector_credential` | A source credential sealed before workspace credentials existed. |
| `destination_credential` | A destination credential sealed before workspace credentials existed. |
| `oauth_token` | The OAuth access and refresh tokens of a source. |
| `ai_provider_key` | The API key of the AI provider. |
| `webhook_secret` | The secret that verifies payloads sent to an inbound webhook. |
| `webhook_signing` | The secret an `action.webhook` call is signed with. |
| `dkim_key` | The private half of a sending domain DKIM key. |
| `test_send_address` | An address on the test-send allowlist. |

## What losing it means

Without the master key nothing sealed can be opened, and the database cannot give it back.

The HMAC root is sealed under the master key, so losing the key also kills every hash made from it:

- Identifier hashes, redaction hashes, suppression hashes and the erasure ledger hashes.
- Every open, click and unsubscribe token already sent (`HmacPurpose::Tracking`).

In practice:

- Suppressions and the erasure ledger hold hashes the new root cannot reproduce. A suppressed
  address is no longer recognised when it is ingested again, and `replay-erasures` cannot match an
  earlier erasure to a person.
- Links in mail already sent stop working: a token that does not verify gets the not-found page,
  so opens and clicks are not recorded and unsubscribe links fail.

A new key does not recover any of this, and it does not start on its own: `serve`, `worker` and
`all` read the stored HMAC root at start and refuse to run when the key cannot open it. The
recovery below removes the unopenable root first. The old hashes stay dead.

### What must be re-created, not re-entered

The sealed values are gone, so create them again:

- Workspace credentials, and any source or destination credential sealed before them: enter the
  secret fields again.
- OAuth tokens: authorize each source again.
- AI provider keys: enter the key again.
- Webhook secrets and signing secrets: create new ones and give the new value to every receiver or
  sender.
- DKIM keys: create new keys, publish the new DNS records, and expect the sending domains to return
  to unverified until they are checked again.
- Test-send allowlist addresses: add them again.

### What keeps working, and what stops

Stored data, segments, flows and templates are untouched and keep working. Sends, syncs and
forwarding stop until their credentials are created again.

## Recovering from a lost key

Do this only when no copy of the key can be found. It deletes rows, so take a database backup
first (for example with `pg_dump`).

1. Stop every `serve`, `worker` and `all` process.
2. Create a new key with `zirv-flow admin generate-master-key --out <path>`, set it as
   `secrets.master_key_file`, and remove `secrets.previous_master_key` and
   `secrets.previous_master_key_file` from the configuration. A start with a previous key and no
   stored root is refused.
3. Delete the HMAC root that the lost key sealed:

   ```sql
   DELETE FROM deployment_keys WHERE name = 'hmac_root';
   ```

   Rows of `encrypted_secrets` stay: nothing at start reads them, and the next steps replace them.
4. Start one `serve` or `worker` process. With no stored root and no previous key it creates a new
   root from the new key and logs `master key loaded` with the new key id. Then start the rest.
5. List the secrets the lost key still wraps. The new root is sealed by the new key, so the query
   reads the new key id from it:

   ```sql
   SELECT workspace_id, purpose, count(*) FROM encrypted_secrets WHERE key_id <> (SELECT key_id FROM deployment_keys WHERE name = 'hmac_root') GROUP BY workspace_id, purpose ORDER BY workspace_id, purpose;
   ```

6. Re-create each one through the web UI or the API, and run the query again. A row that is
   replaced or deleted leaves the list:
   - Workspace credentials, including those sealed before them (`connector_credential`,
     `destination_credential`): rotate the credential with the new secret values.
   - OAuth tokens: authorize the source again.
   - The AI provider key: save the key again in the assistant settings.
   - Webhook secrets: rotate the webhook's secret and give the new value to the sender.
   - Signing secrets: rotate the workspace's signing secret twice. The second rotation deletes the
     first retired one. Give the new value to every receiver.
   - DKIM keys: delete the sending domain and add it again, then publish its new DNS records.
   - Test-send allowlist addresses: remove each entry and add it again.
7. When the list is empty, `zirv-flow admin rotate-master-key` works again. Until then it aborts
   and names the lost key id.

## Detecting a lost or wrong key

- `serve`, `worker` and `all` refuse to start without a master key.
- A start with a key that does not open the stored HMAC root fails with an error that begins
  `the HMAC root:` and names the id of the key that sealed it, which tells you which key to look
  for. The `replay-erasures` command, which also reads the root, fails the same way.
- `zirv-flow admin rotate-master-key` aborts, changing nothing, and names the key id of any row
  wrapped by a key you have not configured. A write from a process whose key is out of date is
  refused with a message to restart it with the current key.
- A healthy start logs `master key loaded` with the key id. Compare that id with the id of the key
  you expect.

## Escrow

Keep the master key apart from the database backups: in a secret manager or an offline copy that
the people who can read the backups cannot reach. A backup together with its key is the whole
secret store.

A restore needs the key from the same era as the backup. If the key was rotated after the backup
was taken, the backup holds rows wrapped by the earlier key, so keep every key a backup can still
hold until the last such backup has expired.

## A compromised key

Rotate the master key first (below). Then rotate the secrets themselves: a rotation re-wraps data
keys but does not change a secret's value, and old backups still hold the old wraps, which the
leaked key opens. Replace the credentials, tokens, provider keys, webhook secrets, signing
secrets, DKIM keys and allowlist addresses, and retire the old copies.

## Rotation limits

- Rotation runs in one transaction over every sealed row, which suits secrets in the thousands.
- Every `serve` and `worker` process must restart with the new key. A process left on the old key
  cannot write secrets once the rotation has run.
- There is no status command. The count the command prints, the `secrets.master_key_rotated` audit
  entry (key ids and the count only) and a second run that re-wraps 0 are the evidence.
- It never changes a secret's value or an HMAC hash.
