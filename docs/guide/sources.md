# Sources

Use this guide to bring customer records and events into an audience, then check
whether that audience is working from current data. A marketer can read sources,
streams and runs; an admin configures connections, credentials and mappings.

## Worked example: a customer file

Ask your admin to host a UTF-8 CSV at a stable HTTPS address, for example:

```csv
customer_id,email,plan
c-1001,ada@example.test,pro
c-1002,sam@example.test,basic
```

1. In **Settings → Fields**, register a string field named `plan`. Choose
   **Back to <workspace name>**, then in **Data → Sources**, add a
   file source with format `csv` and the file's URL. Complete **Test**, then
   **Streams**; the stream is `records`.
2. In **Set up**, choose `customer_id` as the primary key and enable the stream.
   In **Map**, map `email` to an email identifier and `plan` to the `plan` field.
   Preview the two records and save the mapping.
3. Choose **Run now**. Inspect its counters and any rejects before using the data.
   Find the people under **People** and check the field's source.
4. Set a schedule and a staleness threshold that match when the file is refreshed.
   A later full refresh updates changed rows and sweeps removed rows, subject to
   the sweep limit. Keep primary keys stable when reordering the file.

For a one-off upload use [CSV import](#csv-import); for a push integration use
[inbound webhooks](#inbound-webhooks). Operators should read
[credentials](#workspace-credentials) and [egress](#egress) before connecting.

## Sources

A source is a configured external system that records are read from. Each source has a
type (a source type is one module plus one line in the source registry of
`zirv-flow-connectors`), a name that is unique in the workspace, its settings, an optional
cron schedule in UTC, and an enabled flag. It offers **streams**, such as a table or an
endpoint; a stream is read only when you turn it on, and you choose its sync mode (full
refresh, or incremental with a cursor field), its primary key and a staleness threshold.
A run reads a stream and applies it to people (see Sync runs below).

Open **Sources** in the web UI. The setup wizard walks through choosing a type, filling in its
settings, saving and testing the connection, and discovering and setting up the streams. The
form is rendered from the type's field specs, listed by `GET /api/v1/source-kinds`. Discovering
again refreshes what a source reports and never changes the settings you chose.

**Who can do what.** Any member or `read` key can list and read sources and streams. Creating,
changing, deleting, testing and discovering a source and setting up its streams need a
workspace admin, the owner, or an `admin` key. Every such change is audited (`source.created`,
`source.updated`, `source.deleted`, `source.tested`, `source.discovered`, `stream.configured`)
with ids, names and field names, never a value.

**Credentials.** A source uses a workspace credential (see Workspace credentials) by
`credential_id`, and shows the id and only the names of the fields the credential holds, never a
value. To use another credential, send a new `credential_id` with `PATCH .../sources/{id}`; to change
a secret, rotate the credential. Deleting a source deletes its streams and its account tokens, and
leaves the credential. A server started without a master key answers 503 where a credential has to
be sealed or opened. A source that calls out does so through the guarded HTTP client and the egress policy (see Egress).

**API.** Under `/api/v1/workspaces/{ws}/sources`: `GET` and `POST`; `GET`, `PATCH` and `DELETE`
on `/{id}`; `POST /{id}/test` (returns `{ok, error}`; a failed test is a 200); `POST
/{id}/discover`; `GET /{id}/streams`; and `PUT /{id}/streams/{name}` with `enabled`,
`sync_mode`, `cursor_field`, `primary_key`, `staleness_threshold_seconds` and `mapping`; and `POST /{id}/streams/{name}/preview` (see the Mapping reference). Settings are
checked against the type and, for streams, against what discovery reported; a refusal is a 422
that names each field, for example `incremental` without a `cursor_field`.

### The file source

The `file` source type reads a CSV, newline-delimited JSON or JSON file at an **https** address
and offers one stream, `records`. Its settings are `url` and `format` (`csv`; `json` for a
top-level array of objects; `ndjson` for one object per line). The address may not hold user
info or a fragment, and `http` is refused. Its credential fields are all optional and sealed: a
`token` (sent as a Bearer token), or a `username` with a `password` (Basic), and a `query`, a
query string appended to the address, so the signature of a presigned link stays out of the
plain settings. A signed link that redirects must be given as its final address: a redirect off
the source's origin is refused, not followed.

**How it reads.** The file is fetched in byte-range windows of 4 MiB through the guarded client
(so the egress policy, retries with `Retry-After` and the per-source rate limit all apply), and
a record is parsed as soon as it is complete, so memory stays bounded whatever the file's size.
The server must answer `Range` requests for a file over 10 MiB; a server that ignores `Range`
is read whole when the file fits the 10 MiB response cap, and otherwise the run fails with
`file_range_unsupported`. The checkpoint is the byte offset of the next record, so a run that
lost its lease resumes there and neither repeats nor skips a row. Every later window asks with
`If-Range`; if the file's `ETag` (or `Last-Modified`) or size changed between windows the run
fails with `file_changed`, keeping the units already committed. A record over 1 MiB fails with
`record_too_large`; text that is not valid CSV or JSON fails with `csv_invalid` or
`json_invalid`. A byte-range answer whose `Content-Range`, length or total contradicts itself or the
request fails with `file_range_invalid`, so a short body is never taken for the end of the file.
These codes never quote the file or its address.

**Fields and keys.** For CSV the fields are the header names (blank or repeated names are
refused), all text. For JSON they are the top-level keys of the first 100 records, typed from
the first value that is not null; nested values are reached with a dotted path in the mapping
(see the Mapping reference). Choose the stream's **primary key** in **Set up**: a record's key
is its value, or several values joined as a JSON array. Without a primary key the key is the
record's position in the file (`row:<n>`), which holds only while the file is not reordered.

**Full refresh only.** A file has no cursor, so every run reads it from the start; the content
hashes make an unchanged row cost nothing, and a row removed from the file is removed, with its
event, by the sweep at the end of the next run (see Sync runs).

### Mapping reference

One mapping language serves the CSV import and the sync runs. A mapping is a list of `columns`
rules, an optional `key_column` and, for sync runs only, an optional `event` and an optional
`delete_when` (see Sync runs); a CSV import refuses both.

- **Fields and paths.** A rule's `column` is a field name. For a JSON stream it may also be a
  dotted path: `address.city` selects a key, and a segment of digits indexes an array, as in
  `items.0.sku`. A top-level field whose name holds a dot wins over the path. A value that is a
  string is used as it is, a number or a boolean as its JSON text, and null, a missing step or
  an array or object as an empty cell. A path is accepted when its first segment is a field the
  stream reports; a path that leads nowhere in one record is an empty cell, not an error.
- **Targets.** `identifier` (`email`, `phone`, `anonymous_id`, `external:<namespace>`), `trait`
  by registry key, `list` (a truthy cell adds the person), or `ignore`. A list only adds: a cell
  that turns false later does not remove the member. In a sync run a list must exist when the
  mapping is saved and when each unit is applied (`list_missing` fails the run).
- **Transforms**, in order: `trim`, `lower`, `upper`, `cast`, `timestamp`, `concat`, `constant`,
  `value_map`, `split`. That is the whole set: there is no expression language and no script.
- **Event** (sync runs only; a CSV import refuses it with `event_not_supported`). One event per
  record: `{"name": "order_placed", "timestamp": {"column": "order.at", "transforms": [...]},
  "properties": [{"property": "total", "column": "order.total", "transforms": [...]}]}`. The
  name is a constant, the person is the record's identifiers, the time goes through its
  transforms and is read as a timestamp, RFC 3339 or SQL text, see Timestamps (without one the event happened when the run
  started), and a property is the chain's value (an empty one is left out; at most 50). The
  stream must have a primary key (`event_requires_primary_key`).

Example, a JSON file of customers with an order each:

```json
{
  "columns": [
    {"column": "profile.email", "target": {"type": "identifier", "kind": "email"},
     "transforms": [{"op": "trim"}, {"op": "lower"}]},
    {"column": "profile.name", "target": {"type": "trait", "key": "display_name"}},
    {"column": "flags.news", "target": {"type": "list", "list_id": "..."}}
  ],
  "event": {
    "name": "order_placed",
    "timestamp": {"column": "order.at", "transforms": [{"op": "timestamp", "format": "%Y-%m-%d"}]},
    "properties": [{"property": "total", "column": "order.total",
                    "transforms": [{"op": "cast", "to": "number"}]}]
  }
}
```

**Preview.** `POST /api/v1/workspaces/{ws}/sources/{id}/streams/{name}/preview` (admins only,
because the answer holds values) reads the first records of the stream from the start, up to
`limit` (1 to 20, default 10) or 30 seconds, and returns for each record its identifiers,
fields, lists and event, or the `{field, reason}` codes that refused it. The body may carry a
draft `mapping`, checked exactly as a save is; otherwise the saved one is used. It writes
nothing, is not audited, and runs the same `core::mapping` function as a sync run. In the web UI
open **Map** on a stream; the Map dialog also has an assistant that drafts a mapping for you (see Mapping and manifest assistant). A save or a preview against a stream whose source reported a blank or
repeated field name answers 422 with `code` `blank_header` or `duplicate_header`.

### Sync runs

A **run** reads one stream of a source from a checkpoint and applies the records to people.
Start one with **Run now** on the source page (for every enabled stream that has a mapping, or
for one), with `POST /api/v1/workspaces/{ws}/sources/{id}/runs` (`{}` or `{"stream": "name"}`,
admins only, 202), or let the source's cron schedule start them. A stream needs a **mapping**
before it can run: send it as `mapping` in `PUT .../streams/{name}` (the same language as a CSV
import's, see the Mapping reference, with the stream's field names or dotted paths into them as
columns; a trait must be in the registry and a list must exist), or use **Map** on the stream
in the web UI, which previews the mapping on the first records before it is saved.

**Streaming runs.** A stream in the `stream` sync mode (a Kafka topic) is not read to an end. Its
run goes on for up to a day, worked by a chain of `kafka.consume` jobs, and ends `cancelled` when
you cancel it or turn the stream off, and `failed` with a code on an error that retrying cannot
fix or on the reject threshold. Its counters are cumulative; its `attempt` counts only a lost
worker, not each slice. Reconciliation and the sweep do not apply to it, so `reconcile_schedule`
and a `sweep_limit` other than the default are refused for it. See Kafka sources.

**How a run goes.** The `sync.run` job reads the source in batches, maps each record, and
commits a unit of at least 500 records (a source batch is never split) in one transaction: the
profile changes, the retracted records, the rejects, one decision per record, the counters and
the checkpoint. The transaction is fenced twice. It locks the job's lease, so a worker whose
lease was taken over commits nothing, and it locks the run, so a cancelled run is not applied.
Records are kept under the source id and the stream name, with `last_seen_run` set to the run.
A record the source reports as deleted retracts its contribution.

**Deleting what the source dropped.** A **full refresh** ends with a **sweep**: once the whole
stream is read and committed, the stream's live records that the run did not see are removed
(the person's traits are recomputed, and the person follows the workspace's orphan policy), in
bounded transactions of 200 records, each fenced on the lease and on the run like a unit. A
record counts as seen if the run read it and it still exists in the source: applied, unchanged,
and also refused by the mapping or the store, so a broken mapping never cascades into
deletions. A run that fails, is cancelled, or stops on the reject threshold sweeps nothing, and
an incremental run never sweeps, since it cannot tell a dropped record from an unchanged one.
The sweep runs inside the run, before it is `succeeded`; a crash mid-sweep resumes on the next
attempt and removes each record once. Every removal appends a `sync.record` decision with
outcome `deleted` (reason `source`, `signal` or `swept`; it names the record, never a value)
next to the store's `contribution.removed`, and removing a record deletes the events that were
linked to it (the `source_record_id` field of `events`) in the same transaction. The event a mapping yields for a
record is linked to that record's ledger row, so a sweep, a delete signal, a connector delete or a
reconciliation removes it with the record; first-party, ingest and contact events carry no link
and stay. List membership added by a sync is not removed with the record (lists add only), so a
removed record's person stays on its lists until someone removes them.

**Deleted records.** Besides the source's own delete flag, a mapping can name a column that
marks a record as deleted: `"delete_when": {"column": "is_deleted", "rule": "truthy"}` in the
mapping of `PUT .../streams/{name}`. `truthy` reads a flag (`true`, `1`, `yes`, `x`, ...; a cell
that is not a flag is not a delete), `present` treats any non-empty cell as a delete (a
`deleted_at` time). The column must be a field of the stream, or a dotted path into one such as `meta.deleted` (422
when its first segment is not a field), the signal is read
before the mapping runs so a soft-deleted row is never refused, and a CSV import refuses a
mapping with one (422 `delete_signal_unsupported`), as it does an `event`. The web UI edits it under
**Map**, with the rest of the mapping; **Set up** keeps the mapping as it is.

**Reconciliation.** An incremental stream can also be reconciled: a run of kind `reconcile`
(`POST .../runs` with `{"stream": "name", "kind": "reconcile"}`, **Reconcile now** on the
page, or the stream's `reconcile_schedule`, a UTC cron set in `PUT .../streams/{name}`) reads the
stream from the beginning for its keys only, applies nothing, never moves the stream's cursor,
and then sweeps like a full refresh. A full-refresh stream is refused (422
`reconcile_needs_incremental`), and a firing skips a stream that has an active run.

**Sweep limit.** A sweep that would remove more than `sweep_limit` of the stream's live records that existed before the
run (default 0.5, set per stream in `PUT .../streams/{name}`, above 0 and at most 1; absent resets
it) removes nothing and fails the run with `sweep_limit`. This stops an upstream outage that
returns an empty list, a partial answer, or a changed primary key from deleting people whose
state a later read cannot bring back. A drop at exactly the limit sweeps. An admin authorizes a
genuinely large deletion by raising the stream's limit (1 turns the guard off) and running again;
the change is audited with the limit.

**Events and lists.** A mapping's list rules add the people of the unit to their lists (source
`sync`, with the usual history row and `list.membership` decision), and its event rule applies
each record's event through the event path: validation against the published schema or the
unplanned-name policy, redaction and reject handling as for an event sent over HTTP. Both are
in the unit's transaction, so profiles, events, members and the checkpoint commit together or
not at all. An event's idempotency key is derived from the source, stream, record key and event
name, so reading the same file again records nothing twice (the event counts as a duplicate),
and a record that changed does not rewrite its past events. The event's time is the run's start
unless the mapping reads one; an old file's events are accepted and flagged late. A refused
event is an event reject tied to the run (shown in its detail), and the record counts once under
its worst outcome: rejected, then applied, then unchanged; its profile change stands. A run
with an event fails before reading with `stream_not_keyed` (no primary key) or
`event_key_unavailable` (the worker has no redaction key), and with `mapping_invalid` when the
mapping no longer fits the descriptor (a field that disappeared).

**Checkpoints.** Each run keeps its own checkpoint, so a retried run resumes where it stopped
without reading or applying a committed unit again; the run shows its **attempt** number. An
incremental stream also keeps its cursor on the stream: the next run starts there, while a full
refresh always starts from the beginning. A run that fails on its rejects does not move the
stream's cursor, so after the mapping is fixed the next run reads those records again. Changing a
stream's sync mode, cursor field or primary key, or a source's settings, clears the stream's
cursor, since it belonged to the old ones. Such a change is refused with a 409 (`sync_run_active`)
while a run of the stream (for a source's settings, of any of its streams) is queued or running,
since that run would write the old cursor back: wait for it or cancel it first. Other changes,
such as the staleness threshold, enabled, the name, the schedule or the mapping, stay allowed.

**Cancel** with `POST .../runs/{run_id}/cancel` (admins only; 409 when the run has ended). The
run stops at its next unit: the unit in flight is not applied, and the checkpoint stays at the
last committed one.

**Rejects.** A record the mapping or the store refuses is kept in `ingest_rejects` with the run
id, reason codes, and a sample in which a value is shown only when the stream's mapping sends
that field to a trait the registry marks non-PII; every other value is null and listed as
masked. A run fails with `reject_threshold` when, after at least 100 records, more than 5% were
refused. The units applied before stay applied. Sync rejects follow `events.reject_retention_days` (30 days by default). Live erasure
scans rejects without a person id for matching identifier values; see the
[erasure limits](../people.md#deletion-erasure-and-export).

**Settings are checked again.** Before reading, a run checks the stream's settings against the
descriptor discovery last reported, and fails with `stream_settings_invalid` when they no longer
fit (a cursor field that disappeared, say). A stream without a mapping fails with
`stream_not_mapped`; other codes are `source_error` and `job_failed` (the last attempt failed),
`mapping_invalid`, `list_missing`, `stream_not_keyed`, `event_key_unavailable`, `file_changed` and the other file codes (see the file source), `source_missing`, `stream_missing` and `source_kind_unavailable`.

**Schedules.** A source's cron schedule (UTC) is mirrored to a `schedules` row, kept in step in
the same transaction as the source: clearing the schedule, disabling the source or deleting it
stops the firing. When the scheduler leader fires it, the job queues a run for each enabled,
mapped stream that has no active run; a stream with an active run is skipped. One active run
per stream is enforced by the database. The leader also fails (`job_failed`) a run whose job was
buried by the job system or deleted, at most a minute later, so a lost job cannot keep a stream
blocked.

**Reading.** `GET .../runs?stream=&limit=&cursor=` lists runs newest first, and `GET
.../runs/{run_id}` returns one with its counters, state, attempt, error code, trace id and its
first 100 rejects. Any member or `read` key can read; starting and cancelling are audited as
`sync_run.started` and `sync_run.cancelled` with run ids only. Everything a run changes is under
one trace (`GET .../traces/{trace_id}`): each applied, refused or deleted record has a decision
with the `sync_run` cause and a durable id (the source record, the person, or the reject), never
a value. Metrics: `zirv_flow_sync_runs_total{outcome}`, `zirv_flow_sync_records_total{outcome}`
`zirv_flow_sync_events_total{outcome}` (`accepted`, `duplicate`, `rejected`) and `zirv_flow_sync_run_duration_seconds`, with no source, stream or workspace label.

### The HTTP manifest source

The `http` source type reads any HTTP API that a **manifest** describes: its address, how to
authenticate, how it pages, which field is the incremental cursor, which record marks a deletion and
(optionally) how its fields map to people. The manifest is JSON data stored as the `manifest` field of the source's `config`. It is interpreted, never executed: there is no expression language and no
scripting. A manifest holds no secret; the credential fields below hold those.

Add one in the setup wizard like any source. The **Manifest** field is an editor that checks the text
as you type and shows every problem as a sentence with its place in the manifest (click it to jump
to the line), offers a starter manifest for each kind of authentication, and, once the source is
saved, a **Test with sample** button that reads a few raw records with the stored credential. An assistant panel drafts a manifest from pasted API documentation (see Mapping and manifest assistant).

**A manifest, annotated.**

```jsonc
{
  "version": "v1",                                  // the only version
  "base_url": "https://api.example.com/v1",         // https; no user info, query or fragment
  "auth": { "type": "oauth2_authorization_code",    // see Authentication
            "authorize_url": "https://auth.example.com/authorize",
            "token_url": "https://auth.example.com/token",
            "scopes": ["contacts.read"] },
  "rate_limit": { "requests": 100, "per_seconds": 60 },   // optional; 1-1000 per 1-3600 s
  "headers": { "Accept": "application/json" },      // optional; static; at most 10
  "streams": [{                                     // 1 to 20
    "name": "contacts",                             // unique, 1-256 characters
    "path": "/contacts",                            // starts with /, no //, .., ? or #
    "query": { "limit": 100 },                      // optional static parameters, at most 20
    "records_path": "/data",                        // JSON pointer to the array; "" for a top-level array
    "pagination": { "type": "cursor", "param": "cursor", "next": "/next_cursor" },
    "fields": [ { "name": "id", "type": "string" },        // 1 to 200
                { "name": "email", "type": "string" },
                { "name": "updated_at", "type": "timestamp" } ],
    "primary_key": ["id"],                          // fields of the stream; no positional fallback
    "incremental": { "cursor_field": "updated_at",  // optional
                     "request_param": "since", "cursor_type": "timestamp" },
    "delete_signal": { "path": "status", "equals": "deleted" },   // optional
    "mapping": { "key_column": "id", "columns": [ ... ] }         // optional; see Mapping reference
  }]
}
```

**Authentication.** `type` in `auth` is one of `none`; `api_key` (`in`: `header` or `query`, and `name`);
`basic`; `bearer`; `oauth2_client_credentials` (`token_url`, `scopes`, `client_auth`: `body` or
`basic`); and `oauth2_authorization_code` (`authorize_url`, `token_url`, `scopes`, `client_auth`).
The credential fields are fixed by the type: `api_key` reads `api_key`; `basic` reads `username` and
`password`; `bearer` reads `token`; both OAuth2 types read `client_id` and `client_secret` (optional
for authorization code, since a PKCE client may have none). A missing one fails the test or the run
naming `credential.<name>`. Auth headers cannot be set in `headers`: `Authorization`,
`Proxy-Authorization`, `Cookie`, `Host`, `Content-Length`, `Transfer-Encoding`, `Connection`,
`Range` and `If-*` are refused. An `api_key` header name follows the same rule, except that it may
be `Authorization`. Changing the `auth` section of a manifest disconnects an account
connected earlier, because a refresh token belongs to one client and scope set.

**Credential schema.** `credential` in `auth` is optional and declares the shape of the credential the API
needs, so a manifest source gets the same form, storage and rotation as a built-in one. It holds
either `{"kind": "<built-in kind>"}` (`api_key`, `basic`, `smtp`, `oauth2_client`,
`signing_secret`, `bearer_token` or `tls_client`) or `{"fields": [{"key", "label", "type", "required", "secret", "options"}]}` with
the field rules of Workspace credentials (1 to 32 fields, unique lowercase keys, `options` only on an
`enum`, and only a `string` can be `secret`), never both. The schema must contain every credential
field the auth type reads (above; `client_secret` of the authorization-code grant is optional), or
the manifest is refused with `credential_field_missing`. A field the auth type reads that holds a secret (`api_key`, `password`, `token`, `client_secret`) must be `secret: true`, or the manifest is refused with `credential_field_not_secret`, because a non-secret field is stored in plaintext. A manifest holds no value: the validate
response returns the schema as `credential_schema`, and the credential is created from it as kind
`custom`.

**Pagination.** `none`; `cursor` (`param`, `next`: a JSON pointer in the response); `offset`
(`param`, `limit_param`, `limit` 1-1000); `page` (`param`, `first`, and optionally `size_param` with
`size`); `link_header`. They are the runtime's drivers (see Connector runtime).

**Incremental reads and the checkpoint.** With `incremental`, a read after the first sends the stored
value as `request_param`. The checkpoint is `{"since": <text|null>}` and moves only at the end of a
stream: every batch but the last carries the checkpoint the run started with, and the last carries
the highest `cursor_field` value seen (compared as RFC 3339 instants or as integers per
`cursor_type`; a record with a missing or unparsable value is still read and does not move it).
An API may return records in any order, so a per-page high-water mark could skip records; the cost
is that a run that loses its lease re-reads from where it started, which costs requests and not
correctness, because applying a record twice changes nothing.

**Deletes and a suggested mapping.** A record whose value at `path` in `delete_signal` (a dotted path)
equals the literal is read as deleted, and the run removes it like any deleted record. A stream's
`mapping` is only a seed: when discovery first stores a stream, the seed is written as its mapping if
it passes the same checks as a saved one (traits that do not exist drop it, and the stream is left
unmapped); it never replaces a mapping and is not used on a later discovery. A manifest cannot add
people to a list.

**Validation.** `POST /api/v1/workspaces/{ws}/source-manifests/validate` with `{"manifest": ...}`
(an admin; a body up to 65 KiB) answers 200 with `{valid, issues: [{code, path}], streams,
credential_schema}` and
writes nothing; saving a source with the same manifest answers 422 with the same codes under
`config.manifest<path>`. An issue holds a code and a JSON pointer, never a value. The codes:
`required`, `invalid_type`, `invalid_value`, `out_of_range`, `invalid_length`, `invalid_url`,
`unknown_field`, `expression_not_supported` (a script-like key such as `script` or `eval`, or a string
holding `{{`, `${`, `{%` or `<%`), `unsupported_version`, `manifest_too_large` (64 KiB),
`too_many_streams`, `too_many_fields`, `too_deep` (12 levels), `too_many_headers`,
`forbidden_header`, `invalid_header`, `too_many_query`, `path_invalid`, `invalid_pointer`,
`duplicate_stream`, `duplicate_field`, `invalid_field_name`, `primary_key_empty`,
`primary_key_unknown_field`, `cursor_field_unknown`, `invalid_mapping`, `list_target_not_allowed`,
`invalid_credential_schema`, `credential_field_missing`, `credential_field_not_secret`,
and the mapping codes of the Mapping reference.

**Sample.** `POST .../sources/{id}/manifest/sample` with `{"stream": "...", "limit": 1-20,
"manifest": ...}` (an admin; the draft is optional and checked as a save is) reads from the start,
with the stored credential and account tokens, and returns the raw records. A draft must keep
the saved base URL and `auth` section (otherwise 409 `manifest_draft_auth_changed`, before anything
is opened): save those changes first. It writes nothing and is not audited.

**Connect account.** For an `oauth2_authorization_code` manifest, an admin opens the source and
chooses **Connect account**: the page asks the server for the provider's consent URL
(`POST .../sources/{id}/oauth/start`, a signed-in person, never an API key) and navigates to it. The
server stores a single-use state (hashed, ten minutes) with the PKCE verifier. Register
`<server.public_url>/api/v1/oauth/callback` as the redirect URI at the provider; without
`server.public_url` the start answers 409 `public_url_not_configured`. The callback exchanges the
code through the guarded client and stores the access and refresh token in one sealed secret, then
returns to the source page with a success or failure banner. On any failure (an unknown or reused
state, another person's session, a manifest whose authentication changed since the consent started, a provider error, a refused exchange, a response with no refresh
token) nothing is stored. Refresh tokens that rotate are stored by whoever refreshes (a sync run or a
test), serialized across processes so a single-use token is never used twice. Audit entries
(`source.oauth_started`, `source.oauth_connected`, `source.oauth_failed` with a reason,
`source.oauth_disconnected`) hold ids and reasons only, and the counter
`zirv_flow_source_oauth_total{outcome}` has no other label.

### Database sources

The `postgres` and `mysql` source types read the tables and queries of one schema (PostgreSQL) or
one database (MySQL). CI covers PostgreSQL 16 and 18 and MySQL 8.4; compatibility with other database engines is not promised. Add one in the setup wizard like any source: the form asks for
the host, the port (5432 or 3306 by default), the database, for PostgreSQL the schema (`public` by
default), TLS, an optional CA certificate, a statement timeout and the queries, and the credential
is a user and a password.

**Create a read-only role.** The role's grants are the real guarantee that nothing is written.
Every statement also runs in a read-only transaction, which the server refuses to write in, but do
not depend on that alone. Grant `SELECT` on the tables to read and nothing else:

```sql
-- PostgreSQL
CREATE ROLE flow_reader LOGIN PASSWORD 'choose-a-long-password';
ALTER ROLE flow_reader SET default_transaction_read_only = on;
GRANT CONNECT ON DATABASE shop TO flow_reader;
GRANT USAGE ON SCHEMA public TO flow_reader;
GRANT SELECT ON public.customers, public.orders TO flow_reader;

-- MySQL
CREATE USER 'flow_reader'@'%' IDENTIFIED BY 'choose-a-long-password' REQUIRE SSL;
GRANT SELECT ON shop.customers TO 'flow_reader'@'%';
GRANT SELECT ON shop.orders TO 'flow_reader'@'%';
```

Discovery lists the base tables the role can see in the schema (not views and not other schemas),
so the grants decide which streams exist. More than 1,000 tables is refused: grant the role fewer.

**Streams.** A table is a stream named after the table. A **query** is a stream named
`query:<name>`: enter queries in the **Queries** setting of the source (at most 20; a name of 1 to
64 characters of `a-z`, `0-9` and `_`). A query is one `SELECT` or `WITH` statement of at most
20,000 characters, with no semicolon at the end, whose column names are unique. It must return the
primary key columns and, for an incremental stream, the cursor column. No `ORDER BY` is needed: the
source orders and pages it. Discovery prepares each query (the server parses and plans it and does
not run it) to learn its columns. After discovery, set each stream's primary key and, for an
incremental one, its cursor field in the stream settings; a table's primary key is filled in for
you, and `updated_at` is offered as the cursor when the table has a timestamp column of that name.

**Egress.** A database on a private address needs its range in `egress.allow` (see Egress), for
example `allow = ["10.0.5.0/24"]`. The host is resolved and every address checked at each
connection (a test, a discovery and each run); one refused address refuses the host. The
connection goes to the checked address, and the host name you entered is the name the certificate
must match.

**TLS.** The default, `verify`, requires TLS and checks the certificate against the **CA
certificate** setting (a PEM, which is then the only CA trusted) or, when that is empty, the public
roots compiled into the server. The operating system's certificate store is not used. There is no
plaintext fallback. `off` is accepted only toward non-public addresses (private, loopback,
link-local) that the egress policy allows; a public address always needs TLS, even when it is
allowlisted in `egress.allow`.

**How a table is read.** A stream is read in pages of 500 rows, each in its own short read-only
transaction with the statement timeout set (`statement_timeout_seconds`, 60 by default, 1 to 600).
Rows are ordered by the cursor and then the primary key (incremental) or by the primary key (full
refresh), and the next page starts strictly after the last row of the previous one, so rows that
share a cursor value across a page boundary are neither skipped nor read twice. The checkpoint is
the key of the last row. Consequences to plan for:

- A stream must have a **primary key** (one or more columns): without one it cannot be resumed,
  and the run fails. A table without a primary key can be listed but not read.
- The cursor and primary key columns must be of a type that can be paged on. PostgreSQL: `int2`,
  `int4`, `int8`, `numeric`, `text`, `varchar`, `bpchar`, `uuid`, `date`, `timestamp`,
  `timestamptz`. MySQL: integer types (signed and unsigned), `decimal`, `char`, `varchar`, `text`,
  `date`, `datetime`, `timestamp`. Other types (floats, booleans, binary, JSON) are refused.
- Choose a cursor that only moves forward and is assigned when the row is committed, such as an
  `updated_at` maintained by a trigger, and index it together with the primary key. A transaction
  that commits a row with an older cursor value after the stream has passed it is picked up by the
  next full refresh, not before: that is inherent to reading by cursor.
- **Rows whose cursor is NULL are not read by an incremental stream**; a full refresh reads them.
- A NULL primary key value fails the run (it cannot be a record key).
- Database connectors do not emit native delete records. Full-refresh sweeps, incremental
  reconciliation and mapping delete signals handle removals; see [Sync runs](#sync-runs).

**Values.** Every selected column is read as text and arrives as a JSON string, or `null` for SQL
`NULL`; the mapping's cast transforms turn it into numbers, booleans and timestamps (the text
forms PostgreSQL and MySQL give are read as they are, see Timestamps under the mapping). Timestamps are
read in UTC. JSON columns arrive as JSON text. Binary columns (`bytea`, `BLOB`, `BINARY`) are not
text and arrive as `null`.

**Errors and privacy.** An error from the database can quote the value that broke a constraint, so
no server or driver message is ever shown, logged or stored: a failure is one of a few fixed
messages (the credential was refused; the database could not be reached or did not answer in time;
the connection could not be secured; the query failed; a write was refused), and the log line
holds the source type and the SQLSTATE or MySQL error number only. Metrics (no source, stream or
workspace label): `zirv_flow_source_db_batches_total{source_type}` and
`zirv_flow_source_db_errors_total{source_type,reason}` with `reason` one of `auth`, `unreachable`,
`egress`, `timeout`, `tls`, `read_only`, `config` or `query`.

### Kafka sources

The `kafka` source type reads JSON messages from Kafka topics. A **stream is a topic**, and its sync mode is **streaming**: instead of
reading to an end, a consumer follows the topic for as long as the stream is on. Add one in the
setup wizard like any source.

| Setting | Meaning |
| --- | --- |
| `brokers` | One to ten `host:port` pairs, comma-separated, without a scheme. IPv6 addresses are not supported. |
| `topics` | One to twenty topic names, comma-separated. Each is a stream. |
| `security` | `tls` (default) or `plaintext`. |
| `sasl` | `none` (default), `plain`, `scram_sha_256` or `scram_sha_512`. |
| `tls_ca` | A PEM bundle to trust instead of the system roots, for a private authority. |
| `max_lag` | How many messages, over all partitions, the stream may be behind and still count as fresh. Default 10,000. |

The credential fields are `username` and `password` (needed with `sasl`) and `tls_client_cert`
and `tls_client_key` (a client certificate and its key, both or neither). All four are secret:
sealed like any credential and never returned. OAUTHBEARER is not supported.

**Messages.** The **message key** is the record key, as UTF-8 text of 1 to 512 characters, and the
stream's primary key is the virtual field `_key`. The value must be a JSON object of at most 1 MiB;
its top-level fields are the stream's fields, plus two virtual ones: `_key` and `_timestamp` (the
message time, RFC 3339). Those two names win over fields of the same name in the value. Map them
like any field. Discovery reads up to 50 messages from the start of each topic to list the fields;
an empty topic reports only the two virtual ones, so discover again once producers write.

**Deleting.** A message with a **null value** (a tombstone) removes the record with that key and
its linked events, through the same path as a source that reports a deletion. A tombstone for a
key the stream never saw does nothing. A delete signal in the mapping works as for any stream.
There is no sweep and no reconciliation: they cannot tell a dropped record from a quiet one.

**Refused messages.** A message that is not a record is a **reject** of the run, with its place
(`partition 3 offset 1042`) and a code, and never its content: `kafka_key_missing`,
`kafka_key_not_text`, `kafka_value_not_json` (an empty value counts), `kafka_value_not_object`,
`kafka_message_too_large`. The partition goes on. A run over the reject threshold stops like any.

**Delivery.** Offsets are kept in the database, one row per partition, never in Kafka: the consumer
joins no consumer group and commits nothing to the broker. They are written in the same
transaction that applies the messages they cover, fenced on the job's lease and on the run, so
**each committed batch applies once and an uncommitted batch is read again** after a crash, a lost
lease or a restart. A partition with no stored offset starts at its earliest offset, and new
partitions are found at the start of every slice. Order is kept within a partition; a key that
appears in several partitions has no defined order. Two messages with one key are never merged:
each is its own unit of work, so each yields its own event, and a message replayed after a crash
yields the same event again, which is dropped as a duplicate. Offsets that hold no record
(transaction markers, compacted-away batches) are skipped, so compacted topics and transactional
producers do not stall a partition. Messages of aborted transactions are **delivered** (the client
cannot filter them), so a topic written by aborting transactional producers is not supported. If
retention removed the stored offset, the consumer jumps to the earliest one and counts it in
`zirv_flow_kafka_offset_resets_total`. If the stored offset is past the end (the topic was
deleted and created again) the run fails with `kafka_offset_out_of_range`; stored offsets cannot
be reset from the app yet.

**Starting and stopping.** Turning on a streaming stream that has a mapping starts its consumer.
**Run now** and the source's cron schedule start one when none is running, so the schedule is also
the restart path after a failure; `*/5 * * * *` is a sensible choice. Turning the stream or the
source off cancels the consumer. The run stays `running` for up to a day and is worked by a chain
of `kafka.consume` jobs of 60 seconds each; each slice enqueues the next, in the transaction that
completes it. A day after it started, the run ends `succeeded` and a new run takes over, so its
trace, counters and reject ratio stay small. A consumer holds one worker slot during its slice: with
more streaming streams than `worker.concurrency - 1`, other jobs starve. Watch the running and ready job counts in `zirv_flow_jobs{kind,state}`.

**Lag and freshness.** After each slice (and every 10 seconds within one) the consumer stores the
log's end for each partition. **Lag** is the end minus the next offset to read. `GET
.../sources/{id}/streams` carries `lag` in `freshness` with the `total` and each partition's `lag`
(counts, which members may read), and the streams table shows `Lag 1,204` next to the badge. Lag
within `max_lag` counts as a success and lag above it as a failure, so a stream that falls behind
turns **failing**, and a consumer that stops turns **stale** by the usual time rule, which needs a
staleness threshold on the stream or a schedule on the source. The lag of a transactional topic can
include its trailing commit marker, which is never delivered, so `max_lag` should not be 0 for one. The gauge
`zirv_flow_stream_lag_messages{workspace,stream}` follows it, and `zirv_flow_kafka_fetches_total{outcome}`
counts fetches.

**Network.** The client has no resolver hook, so every connection goes through a loopback SOCKS5
relay that resolves the host, checks every address against the egress policy (see Egress), dials
the checked address and leaves TLS to verify the host name. This covers the bootstrap hosts and
every broker the cluster names, on every reconnect. A broker whose address is refused fails the
read with `source_error`, counted in `zirv_flow_egress_denied_total`. While a read runs, another
process on the same machine could use the relay to reach any destination the policy allows. A broker
that is private needs an `egress.allow` entry like any private destination. Errors are static
codes (`kafka_connect_failed`, `kafka_tls_failed`, `kafka_auth_failed`, `kafka_topic_missing`,
`kafka_timeout`, `kafka_compression_unsupported`) and never a broker's own text. The client logs host names, never messages. The
SASL password lives in plain memory inside the client while it runs and cannot be wiped there.

**Limits.** JSON only (no Avro, Protobuf or schema registry), no consumer groups, no producing,
no topic patterns. Gzip, Snappy and LZ4 compression only: zstd-compressed topics are not read in 1.0, and
a zstd batch fails the read with `kafka_compression_unsupported`. Zstd returns once the Kafka client
releases a version on zstd 0.14.3 or later (#338). At most 1 MiB is fetched per partition per request. Messages are checked after
decompression only for the 1 MiB record limit.

### Source freshness

Marketers can see whether the data behind a segment is current. Every enabled stream of an
enabled source is **fresh**, **stale** or **failing**, and the state is worked out from facts on
each read, never stored, so a changed threshold or schedule takes effect at once.

- **Failing:** the last run of the stream failed (any error code, including `reject_threshold`
  and `job_failed`). This holds however recent the last success was. A cancelled run is not a
  verdict on the data and changes nothing. The next success clears it.
- **Stale:** the time since the last success is past the stream's threshold. Exactly at the
  threshold is still fresh.
- **Fresh:** anything else. A stream that has never run is fresh until the threshold has passed
  since it was enabled, then stale. Enabling a stream again restarts that grace period, and
  streams that were already enabled when the feature arrived start theirs at the upgrade.

**The threshold.** A stream's own `staleness_threshold_seconds` wins. Without one it is twice
the source schedule's interval (an hourly schedule is stale after 2 hours); the interval is the
longest gap among the next 8 slots, so a weekdays-only schedule allows for the weekend. A stream
with neither a threshold nor a schedule is **never stale**, though it can still be failing.
Admins set or clear the threshold with the stream's `PUT .../streams/{name}` or the stream
dialog; it is audited as `stream.configured` with the number (`staleness_threshold_seconds`).

**Where it shows.** `GET .../sources/{id}/streams` carries `freshness` on each stream: `state`,
`last_success_at`, `threshold_seconds`, `threshold_is_default` and `high_water_mark`. It is
`null` for a stream that is not read, or whose source is disabled. `GET .../sources` and `GET
.../sources/{id}` carry `freshness` as the worst state of the source's enabled streams (`null`
when there is none). The high-water mark is the stream's checkpoint, which can hold a value
taken from a record, so only workspace admins get it; everyone else gets `null`. The Sources
list shows a badge per source, and the streams table and the stream dialog show the badge, the
time since the last success and the threshold.

**Metrics.** The scheduler leader refreshes these at most every 30 seconds, with ids as the only
labels: `zirv_flow_stream_freshness_seconds{workspace,stream}` (age of the data),
`zirv_flow_stream_state{workspace,stream}` (0 fresh, 1 stale, 2 failing) and
`zirv_flow_streams{workspace,state}` (how many enabled streams are in each state). A stream that
is disabled or deleted is set to 0, and an instance that stops leading zeroes what it published.

**Other source types.** A pull run reports when it ends. A webhook source reports each accepted
payload through the same call, `record_stream_outcome` (see Inbound webhooks), and a stream consumer
reports through it too: a success at a time, or a failure, written to the same columns. The Kafka
consumer reports within its lag bound as a success and above it as a failure (see Kafka sources). A
webhook has no schedule, so its threshold is the one you set ("expect a payload at least every");
without one it is never stale.

**Sending limit:** stale or failing inputs do not pause sends. Flow publishing shows
them as warnings; the marketer must decide whether to proceed.

## Workspace credentials

A **credential** is a named object that holds the keys and secrets of one external account. It is
entered once, sealed, shared by any number of sources and destinations, rotated in one place, tested,
and shows where it is used. Under `/api/v1/workspaces/{ws}`, all for workspace admins (no access is
404):

| Request | What it does |
|---------|--------------|
| `GET /credential-kinds` | the kinds and their schemas (fields, types, which are secret), so a form can be drawn from data; never a value |
| `GET /credentials`, `POST /credentials` | list the live credentials; create one |
| `GET`, `PATCH`, `DELETE /credentials/{id}` | read it with its `used_by` list; change its name or non-secret `fields`; archive it |
| `PUT /credentials/{id}/secret` | rotate: seal the new secret values, swap them in and delete the old secret in one transaction |
| `POST /credentials/{id}/test` | run the check of the source or destination named in the body (`source_id` or `destination_id`, which must use the credential); answers `{ok, message, code}` with a fixed sentence that names no host, URL or value, and records `last_tested_at` and the outcome |

A schema is a list of fields; each has a `key`, a `label`, a `type` (`string`, `integer`, `enum` or
`url`), `required` and `secret`. The built-in kinds are `api_key` (a key sent as a named header or as
a bearer token), `basic` (username and password), `smtp` (host, port, TLS mode, username and
password), `oauth2_client` (client id, client secret and an optional token URL: a manifest carries
the URL it uses and wins over the credential's), `signing_secret` (the current secret and an
optional `previous_signing_secret` for a rollover), `bearer_token` (a secret `token`) and
`tls_client` (a client certificate and its secret private key, for mutual TLS). A
credential of kind `custom` carries its own schema in the create request, which is how an
implementation described by a manifest gets the same form (see The HTTP manifest source). Create
with `{"kind", "name", "fields", "secret"}`: the non-secret values go in `fields`, every secret value
in `secret`, and a secret value given in `fields` (or the reverse) is refused. All secret fields are
sealed together as one JSON object (purpose `workspace_credential`); the non-secret ones are stored
as plain JSON.

Nothing secret comes back. A credential shows its `fields`, a **hint** per secret field (its last
four characters when the value has at least 16 characters, otherwise nothing, so it is fully masked),
and a **fingerprint**, the redaction HMAC of the sealed object, which the UI uses to warn when the
same secret is entered twice (`same_secret_as`). The audit trail records `credential.created`,
`credential.updated`, `credential.rotated`, `credential.tested` and `credential.archived` with field
names only.

A source or destination takes a `credential_id` instead of inline credential fields, and must use a
credential that holds every credential field its type requires. It opens the current secret right
before each use, so a rotation reaches all of them on their next call and nothing else changes.
Deleting a source does not delete the credential. Archiving is refused with 409 `credential_in_use`
while a live source or destination uses the credential, and an archived credential cannot be
referenced (409 `credential_archived`). Credentials a source or destination had sealed on its own
before this existed became credentials of kind `imported` when the database was migrated: they point
at the same encrypted row, are never re-sealed by the migration, keep working, and are re-sealed as
`workspace_credential` on their next rotation (which must give the same field names). OAuth access
and refresh tokens stay per source, because they are runtime state. In the web UI, **Settings → Credentials**
lists, creates, edits, rotates, tests and archives them, and a source's
credential field is a picker with **New credential**.

## Egress

Outbound source, destination and assistant traffic is checked against the egress
policy. The guarded HTTP client refuses non-public addresses: loopback, private ranges, link-local,
carrier-grade NAT, cloud metadata, multicast and other special ranges are all blocked, whether the
URL names them directly, a host name resolves to them, or a redirect points at them. A host name
that resolves to a mix of allowed and blocked addresses is refused whole. Proxy environment
variables (`HTTP_PROXY`, `HTTPS_PROXY`) are ignored by this client.

To reach an internal system on purpose, list it in `egress.allow` as CIDR ranges:

```toml
[egress]
allow = ["10.0.5.7/32", "192.168.0.0/16"]
```

or `ZIRV_FLOW_EGRESS__ALLOW='["10.0.5.7/32", "192.168.0.0/16"]'`. Entries are CIDR only (no host
names or ports), and an invalid entry fails startup and `zirv-flow config check` with the key and
the reason. A cloud metadata address such as `169.254.169.254` (the infrastructure address `168.63.129.16`) is unlocked only by an entry
naming exactly that address (`169.254.169.254/32`); a broader range never unlocks it. Each refusal
increments `zirv_flow_egress_denied_total{class}` and logs the host and class. Sign-in with an OIDC
provider is not covered, because the operator configures that URL.

## Inbound webhooks

A webhook lets an external system (a form tool, a shop back end, an internal service) push one JSON
document at a time to Zirv Flow. It is a **push source**: a source of type `webhook` with one stream,
`payload`, so it has the same mapping editor, preview and freshness as any other stream. Its
payloads are not read by a run; each one is verified, queued in the durable inbox and mapped to a
person, an event and list memberships by `ingest.drain`.

**Set up.** Under Sources, add a source and choose Webhook (or `POST .../webhooks`, admins only).
You give it a name, a **dedupe field** (a dotted path into the payload whose value identifies it,
such as `id` or the nested path `data → order → id`) and, optionally, how often you expect a payload. The response
carries the **URL** (`/hooks/{id}` on your server's origin) and the **shared secret**
(`zfwh_` and 64 hex digits). The secret is shown once, in that response, and never again: it is
stored sealed (purpose `webhook_secret`), and no read, update, audit entry or log line holds it.
Rotate it with `POST .../webhooks/{id}/rotate-secret`; the old secret stops working at once. The
id in the URL only locates the hook, it is not a secret, and the signature authorizes.

Then map it: open the stream (Set up, then Map), paste a sample payload, and the mapping editor
shows what the mapping makes of it, with the same function the drain calls and without writing
anything (`POST .../streams/payload/preview` with `sample`: 1 to 20 objects, 64 KiB in all).
Columns name payload fields, a nested one with dots (`order.total`, `items.0.sku`); a field a
payload lacks is an empty cell. A webhook stream has no key column and no delete signal, and an
event needs no primary key: the event id comes from the dedupe field. Turn the stream on. Until it
is on and mapped the URL answers `404`.

**The signature.** Each request carries an HMAC-SHA256 of its body, keyed with the secret's UTF-8
bytes, as the prefix followed by 64 hex digits (either case). The defaults, all per hook:

| Setting | Default | Meaning |
|---|---|---|
| `signature_header` | `X-Zirv-Signature` | the header holding the signature |
| `signature_prefix` | `sha256=` | the text before the digest; may be empty |
| `timestamp_header` | `X-Zirv-Timestamp` | unix seconds as a decimal number; `null` selects the body-only scheme |
| `tolerance_seconds` | 300 | how far the timestamp may be from the server's clock, either way (1 to 86400) |

The **timestamped scheme** signs `"{timestamp}.{body}"` (the timestamp text exactly as sent, a dot,
the raw body bytes) and refuses a timestamp outside the tolerance. The **body-only scheme** signs the
raw body. It exists because many senders cannot sign a timestamp; its only replay defense is the
dedupe field. Other algorithms, base64 signatures and handshake protocols are not supported.

```sh
# The timestamped scheme (the default).
BODY='{"id":"order-1","email":"ada@example.com"}'
TS=$(date +%s)
SIG=$(printf '%s' "$TS.$BODY" | openssl dgst -sha256 -hmac 'zfwh_example' | sed 's/^.* //')
curl -X POST 'https://flow.example.com/hooks/3f2a9c1e-0b7d-4e55-9c1a-7d4e2b6a8f10' \
  -H 'Content-Type: application/json' \
  -H "X-Zirv-Timestamp: $TS" \
  -H "X-Zirv-Signature: sha256=$SIG" \
  -d "$BODY"

# The body-only scheme.
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac 'zfwh_example' | sed 's/^.* //')
curl -X POST 'https://flow.example.com/hooks/3f2a9c1e-0b7d-4e55-9c1a-7d4e2b6a8f10' \
  -H 'Content-Type: application/json' -H "X-Zirv-Signature: sha256=$SIG" -d "$BODY"
```

With the secret `zfwh_example`, the timestamp `1700000000` and the body above, the signature is
`c8447361d430f2281aef6579aec897c4930f3d15af1920d3da1f83ff8e4f4d56`; a unit test signs that
example with the same recipe.

**What happens to a request.** In this order, and nothing is written before the signature checks
out: a declared body over `ingest.max_webhook_bytes` is `413`; an unknown id, or a hook that is not
receiving (source or stream off, stream not mapped), is `404`; a hook over `ingest.rate_per_hook`
is `429` with `Retry-After`; the body is read with the same cap (`413`); the signature and, for the
timestamped scheme, the timestamp are checked, the digest compared in constant time (`401`, code
`invalid_signature`, one answer for every cause but a stale timestamp, which says so); the body
must be one JSON object (`400`, `invalid_payload`; arrays and several records per request are not
supported); the dedupe field must hold a string or number of 1 to 128 characters (`422`,
`dedupe_missing`); then the payload is appended to the inbox and the answer is `202
{"accepted":1}`: durable, not yet applied. A database failure is `503` and stores nothing, so
retry; so is a server started without a master key. The rate limit is a token bucket per hook and
per `serve` process (`ingest.rate_per_hook`, `ingest.burst_per_hook`), charged only for ids that
belong to a hook, so a flood of made-up ids costs one indexed lookup each and no memory. Anyone
who holds the URL can use up a hook's budget with unsigned requests; limit senders by address at
your edge proxy.

**Dedupe and replay.** The dedupe value becomes the event's message id, namespaced by the hook
(`wh:{hook}:{value}`), so the same payload sent again, inside or outside the tolerance window, with
a new signature or the same, leaves one event and the same trait values. The profile record is
keyed by the person's strongest identifier and observed at the signed timestamp (at the arrival
time for the body-only scheme), so a replayed older payload never overwrites a trait that a newer
one set. With the body-only scheme a captured request can be replayed, but only to the same effect.

**Mapping failures.** A payload the mapping refuses (no identifier, a value that does not cast) is
an ingest reject for the stream, with field names, codes and a masked sample and never a value
outside non-PII traits, and the rest of the batch applies. A payload queued before the stream lost
its mapping is rejected as `mapping_unavailable`. Mapping runs in the drain, so an edit to the
mapping applies to payloads still queued. A webhook reject is not replayable.

**Freshness.** An accepted payload reports through `record_stream_outcome`, at most once a minute
per hook per process, so the stream shows **fresh**, **stale** (no payload within the stream's
threshold; set it with "expect a payload at least every" or `staleness_threshold_seconds`) or
**failing** like any other (see Source freshness). A rejected request writes nothing to the stream.
A webhook source is created, changed and deleted only through `/webhooks` (the generic source
routes answer `409`, code `managed_by_webhooks`) and never gets a run (`409`, `push_source`).

**Metrics.** `zirv_flow_webhook_requests_total{outcome}` with `accepted`, `not_found`,
`rate_limited`, `too_large`, `signature_missing`, `signature_malformed`, `signature_stale`,
`signature_mismatch`, `invalid_payload`, `dedupe_missing` or `unavailable`; the drain's
`zirv_flow_ingest_applied_total` carries `kind="webhook"` with the outcomes `applied`,
`unchanged`, `rejected`, `apply_failed`, `accepted` and `duplicate`. No hook id, workspace or
value is a label, and nothing logs a body, a header value, a signature or a secret.

## Connector runtime

Every HTTP source makes its calls through the connector runtime in `zirv-flow-connectors`
(`SourceContext::send`), which adds the pieces below on top of the guarded client, so no source
type implements them itself. A test connection, a discovery and a sync run of one source all use it.

**Auth.** API key (in a header or a query parameter), basic, bearer, and OAuth2 client
credentials and the OAuth2 authorization-code grant with PKCE and refresh (the client proves itself in the form body or with basic auth). The plaintext
credential is borrowed for the duration of a call. An OAuth2 token is fetched when there is none or
when less than 60 seconds of it remain; a `401` on a token that was already cached refreshes it
once and retries the call once, and a second `401` is a refused credential. A refreshed client-credentials token is
handed back to the caller (`take_refreshed_token`); the runtime never writes to the database. For
the authorization-code grant the refresh token is the source's long-lived credential: the
code is exchanged once at consent, and a refresh that returns a new refresh token (rotation)
replaces the old one. A caller that persists tokens gives the context a token store, and the
refresh then runs through it so that processes refreshing the same source take turns: the api and
the sync worker lock the source's token secret, reload it, call the provider only if nobody else
has, and save the result in the same transaction. Both tokens live in one sealed secret per source,
linked from the source and never returned. Auth is attached only to requests for
the source's base origin (scheme, host and port); a request for any other origin, such as a
pagination link that points elsewhere, is refused before it is sent. Redirects are followed only
within the same origin: a redirect to another origin, including `http` to `https` on the same host,
ends the call with the same error, so a header or query key never leaves the origin. Give such a
source its final base URL.

**Retries.** `GET` and `HEAD` (and a request the source marks idempotent) are retried on `429`,
`500`, `502`, `503`, `504` and on connection failures and timeouts, up to 5 attempts, with an
exponential backoff from 1 s to 60 s with jitter. On `429` and `503`, `Retry-After` (seconds or an
HTTP date) is used instead; a `Retry-After` over 300 s ends the call at once without waiting. Other
`4xx` codes and a `POST` are not retried. A call that runs out of attempts fails with the last
status.

**Rate limit.** A source type may declare a request rate; the runtime holds a token bucket per
source and waits for it before each request. The limit is **per process**: it is shared by the sync
runs, test connections and discoveries of that source in one process, but several workers each have
their own bucket and can exceed the limit together. Honouring `Retry-After` reduces that overshoot
and does not prevent it. A source type may declare the rate once for all its sources, and an `http`
source declares it in its manifest; editing it takes effect on the next call. This source-request limit is distinct from the shared destination rate bucket used for delivery.

**Pagination.** Four drivers read an API to its end as a stream of pages: cursor (the next cursor
at a JSON pointer, a missing, null or empty cursor ends it), offset (ends on a page shorter than
the limit), page number (ends on an empty page, or a short one when a page size is set) and the
`Link: rel="next"` header. A cursor or link that repeats ends with an error rather than looping,
and so does a response whose records are not an array, or a cursor or link longer than 8 KiB. An
offset or page stream whose API ignores the parameter and sends the same full-size page three times
in a row ends with the same error (two identical pages can be genuine, so they are read); with no
page size set, only the 100,000-page cap ends such a stream.

**Metrics and logs.** `zirv_flow_source_requests_total{source_type,outcome}`,
`zirv_flow_source_retries_total{source_type,reason}` and
`zirv_flow_source_rate_limit_wait_seconds_total{source_type}`. The labels are the source type and
fixed values only: never a URL, a host or a source id. No error or log line holds a secret, a URL,
a header or a response body.

## CSV import

Load people from a CSV file in four steps: upload it, map its columns, check the mapping with
a dry run, then run it. Open **Imports** under **Data** in the web UI, or use the API under
`/api/v1/workspaces/{ws}/imports`. An editor or `write` key can upload, map and dry-run; starting an import needs a publisher
or `publish` key. Reading needs workspace access or a `read` key. Sessions send the CSRF token as for
every other write.

**The file.** UTF-8 text (a byte order mark is ignored), RFC 4180 quoting (quoted values may
hold the delimiter, quotes written twice and line breaks), a header row of unique, non-blank
names, and the delimiter detected from comma, semicolon and tab unless one is given. The
upload is `POST .../imports?name=...` with `Content-Type: text/csv` and the file as the raw body;
`file_name`, `source`, `stream` and `delimiter` (`comma`, `semicolon` or `tab`) are optional.
The body is streamed, never held whole: it goes into PostgreSQL in 1 MiB chunks while its
SHA-256 and row count are taken, so separate `serve` and `worker` processes need no shared
disk. A file over `imports.max_file_bytes` (512 MiB by default) is a 413, a header with a
blank or repeated name or a file that is not UTF-8 is a 422, and a refused or interrupted
upload keeps nothing: the import exists as `uploading` (hidden from the history) while the body arrives, each chunk is a statement of its own so no transaction stays open, and an upload that breaks off, is refused or sends no byte for 60 seconds (408) is deleted with its chunks. The
answer is the import with its header and the first 20 rows; the cells of those rows are null
and `samples_masked` is true for callers without personal-data access.
Row numbers are the ones a spreadsheet shows: the header is row 1, every blank line counts as a row, and a quoted value over several lines is one row.

**The mapping.** (The same language serves the sync runs of sources; see the Mapping reference.) `PUT .../imports/{id}/mapping` saves it and moves the import to `mapped`:

```json
{
  "key_column": "customer_id",
  "columns": [
    {"column": "Email", "target": {"type": "identifier", "kind": "email"},
     "transforms": [{"op": "trim"}, {"op": "lower"}]},
    {"column": "Joined", "target": {"type": "trait", "key": "joined_at"},
     "transforms": [{"op": "timestamp", "format": "%d/%m/%Y"}]},
    {"column": "Newsletter", "target": {"type": "list", "list_id": "..."}}
  ]
}
```

A column becomes an `identifier` of a kind (`email`, `phone`, `anonymous_id` or
`external:<namespace>`), a `trait` by registry key (the registry supplies the type), a `list`
(a truthy cell such as `true`, `1`, `yes`, `y`, `t`, `on` or `x` adds the person; `false`, `0`,
`no`, `n`, `f`, `off` and an empty cell do not; anything else is `wrong_type`), or is left out.
Between the cell and the target run the transforms, in order: `trim`, `lower`, `upper`,
`cast` (to a trait type), `timestamp` (a strftime `format` such as `%d/%m/%Y %H:%M`, or `unix`
or `unix_ms`; a value without a time zone is read as UTC), `concat` (the value and other
columns, skipping empty ones, with a separator), `constant`, `value_map` (replaces the values
it lists) and `split` (into a list of trimmed, non-empty strings). That is all: there are no
expressions or scripts. An empty cell leaves its trait out. The mapping is rejected with a 422
that names the rule and the column unless it has at least one identifier target, every column
and list exists, every trait key exists and is not archived, no trait is filled by two columns
and every transform is well formed. It can be saved again until the first run starts; saving
clears an earlier dry run.

**Timestamps.** A value that a mapping reads as a timestamp (a `timestamp` trait target, a
`cast` to `timestamp`, an event time) is RFC 3339, or the SQL text form that PostgreSQL and MySQL
produce: `YYYY-MM-DD HH:MM:SS`, a space or a `T` between the date and the time, then optionally
`.fraction` (1 to 9 digits) and an offset (`+HH`, `-HH`, `+HH:MM`, `-HH:MM` or `+HHMM`). A value
without an offset is **UTC**, so a MySQL `DATETIME` or a `timestamp without time zone` must hold
UTC to be right. The date alone, a time without seconds and a value with trailing text are
`bad_timestamp`; use the `timestamp` transform with a `format` for those. CSV imports read
timestamps with the same leniency as sync runs.

**Dry run.** `POST .../imports/{id}/dry-run` queues the `import.dry_run` job. It reads the
whole file with the mapping and writes nothing but the rejected rows (phase `dry_run`) and the
counters: no person, identifier, field or list changes. Poll `GET .../imports/{id}` for
`status` (`dry_running`, then `dry_run_done`) and the counters `rows_total`, `rows_done`,
`rows_applied` (rows that would be imported) and `rows_rejected`.

**Run.** `POST .../imports/{id}/run` queues the `import.run` job; a dry run is not required.
The file is read in batches of 500 rows. Each batch is one transaction that applies the profile
records, adds the list members (source `import`), writes the batch's rejects (phase `run`)
and advances the `rows_done` checkpoint with the counters, so a crash or a lost lease resumes
from the checkpoint when the job is retried and neither skips nor repeats a row. Poll the
import for progress: `rows_done` of `rows_total`, `rows_applied`, `rows_unchanged` (the row
repeats what is stored, so nothing was written) and `rows_rejected`. `POST .../imports/{id}/cancel`
stops it at the next batch boundary; the batches already applied stay applied. One run per
import: starting a second is a 409. A run whose job ran out of attempts is marked `failed` with `job_failed` (rows already
applied stay applied); upload the file again to finish, which changes only what is missing.

**Idempotency.** Records are written under the import's `source` (default `csv`) and `stream`
(default the file name without its extension). A row's record key is the value of the
`key_column`, or, without one, the file's SHA-256 and the row number. Running the same file
again with the same source and stream therefore changes nothing: the keys are equal and the
content hashes make every write a no-op. A file that changes between runs needs a key column
to update the same records; without one every row of the new file is a new record. Records
are observed at the time the run was started, so with the default precedence a later import wins where fields conflict.
Keep the source and the stream stable for data you import repeatedly.

**Rejected rows and the threshold.** A row the mapping or the profile store refuses is
rejected, not applied, and the import goes on. The reasons are codes: `missing_identifier`,
`invalid_identifier`, `wrong_type`, `bad_timestamp`, `unknown_column`, `invalid_key` (a key
over 512 characters) and `apply_failed`. They name the column and never a value. Once more
than 1,000 rows are done and more than 5% of them were rejected, the run stops as `failed` with
`error_code` `reject_threshold`, so a broken mapping cannot flood a workspace. The batches
applied before it stopped stay applied; fix the mapping and run the file again, which changes
only what is still missing. Other failure codes: `list_not_found` (a mapped list was
deleted), `file_missing` (the file was already deleted), `file_unreadable`, `job_failed` and `abandoned`.

`GET .../imports/{id}/rejects?phase=&limit=&cursor=` pages through the rejects in row order
with `row_number` and the codes (`limit` 1 to 200). `GET .../imports/{id}/rejects.csv?phase=` downloads them as
`row_number,column,reason`, one line per problem. For callers with personal-data access (`view_pii` or `pii`, implied for owners,
admins and `admin` keys), each row's own cells follow under the file's column names, in a file that may hold
personal data; cells that start like a spreadsheet formula get a leading `'`. Everyone else
who may read gets the codes alone. Each download is audited.

**Retention and personal data.** The cells of rejected rows are the one place the file's
values can be retained beyond the people they created: they require personal-data access and are deleted after the
configured window (`retention.import_reject_days`, default 30 days) from when they were written. The scheduler leader deletes the uploaded file after the configured window (`retention.import_file_days`, default 7 days) once an import ends
(succeeded, failed or cancelled) and the rejects at their window. An import that is uploaded, mapped or dry-run and untouched for 7 days is cancelled as `abandoned` and loses its file at once, and an upload that never finished (a crash mid-body) is deleted after a day.
The import itself, its mapping and its counters are kept. Live erasure also scans files and rejected cells by identifier value; unmatched spellings or
embedded non-email identifiers can survive until retention. See the
[erasure reference](../people.md#deletion-erasure-and-export). The
audit log records `import.created`, `import.run_started`, `import.cancelled` and
`import.rejects_downloaded` with ids and counts only. No log line, metric label, decision or
audit entry holds a cell, an identifier or a trait value.

## Mapping and manifest assistant

Open **Map** on a stream (in the setup wizard or later) or the manifest editor of an `http` source,
say what you want and, for a manifest, paste the API documentation. The panel is empty while the
assistant is off. The server asks the model to call one draft tool, checks each answer with the
same rules a save applies, and feeds the problems back at most `ai.max_repair_attempts` times. You
get either the problems of the last attempt (no draft, nothing to accept) or a validated draft with
its field-by-field difference from the saved version and a dry run: the mapping preview on five
records (or on your pasted sample, for a webhook stream), or a five-record sample of the
manifest's first stream. A manifest dry run needs a saved source whose base URL and `auth` the draft
keeps; otherwise the panel says why it did not run.

- **Privacy.** With the personal-data opt-in off, only field names, types, trait keys, list names
  and your prompt are sent, and the source is not read for the model. With it on, up to 5 sample
  records (8 KiB) are added. Field names, pasted documentation and records are sent as fenced data
  that the model is told never to follow. Do not paste personal data into the prompt.
- **Accept loads, Save publishes.** Accept checks the draft again and loads it into the editor as
  the ordinary unsaved draft. Saving it stays the existing Save, with its permission and audit. A
  draft expires after 7 days, is yours alone, and goes with its source.
- **Routes.** `POST /api/v1/workspaces/{ws}/ai/drafts` (`kind`, `source_id`, `stream`, `prompt`,
  `documentation`, `sample`) answers 201 with `status` `valid` or `invalid`. It needs an admin and a
  signed-in user (an API key is 403 `ai_user_required`). Problems: 409 `ai_disabled`, 402
  `ai_budget_exceeded`, 429 `ai_rate_limited`, 502 `ai_provider_error`, 422 naming the field
  (`sample_required`, `sample_not_supported`). `POST .../ai/drafts/{id}/accept` answers 200 with the
  content, 409 `ai_draft_invalid` or `ai_draft_outdated` (the saved version changed), 422 when it no
  longer passes a save's rules, 404 for another user's or an expired draft. Accept is audited as
  `ai.draft.accepted`.
- **Metrics.** `zirv_flow_ai_draft_runs_total{kind,outcome}` (`valid`, `invalid`, `failed`),
  `zirv_flow_ai_drafts_accepted_total{kind}`, and the existing `zirv_flow_ai_calls_total` under the
  features `mapping`, `manifest`, `segment`, `segment_explain` and `evaluation`.
- **Timeouts.** A run is one synchronous request of up to (1 + `ai.max_repair_attempts`) calls. A
  reverse proxy needs a read timeout of at least (1 + `ai.max_repair_attempts`) x 60 s + 30 s for
  `POST /api/v1/workspaces/{ws}/ai/drafts`.
- **Evaluation.** `zirv-flow admin evaluate-assistant --workspace <slug> [--case <name>]` runs the
  golden prompts against the workspace's model through the gated dispatcher (every prompt counts
  against the budget) and prints one JSON line per case with the properties that held, the ones
  that did not, attempts, tokens, cost and model. It exits 0 whatever the score. CI runs the same
  cases on a fake provider and never calls a model.

## Limits

- File sources do full refreshes only. Large files need byte-range support;
  individual records are capped at 1 MiB. A changed file fails the run rather
  than silently mixing versions.
- Database cursors must advance reliably; incremental reads miss NULL cursors
  and late commits behind the checkpoint. Use reconciliation for removed keys
  and a full refresh to reread values missed behind the cursor.
- HTTP manifests describe data and authentication; they cannot execute scripts.
  Retries and source rate buckets are per process.
- Kafka uses JSON objects and database offsets, without consumer groups. Aborted
  transactional records cannot be filtered, and the app cannot reset offsets.
- Accepted webhook requests are durable but asynchronous. A 202 does not mean
  the mapping or event schema accepted the payload.
- Mapping list targets only add members. A sweep does not undo list membership.
  Freshness is a warning, never an automatic stop on sending.

See [segments](segments.md) to build an audience from the resulting data and
[channels](channels.md) to deliver to it.
