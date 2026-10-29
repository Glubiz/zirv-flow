# People and event reference

Use this reference to inspect profiles, integrate ingestion and govern retained
events. For connecting external data use [sources](guide/sources.md); for audiences
use [segments](guide/segments.md). Permissions follow the
[account and workspace roles](operations/access.md#accounts-workspaces-and-authentication).

## People and fields

The profile store holds the people of a workspace, the identifiers they are known by, and
the fields (traits) sources fill in on them. Sources, ingestion, contacts and CSV imports
all feed the profile store; the field registry controls how their values combine.

**The field registry** lists every field a workspace knows: a key (`plan_tier`), a type, a
precedence rule and a few flags. Admins manage it under **Fields** in the web UI or through
`GET` and `POST /api/v1/workspaces/{ws}/traits` and `PATCH .../traits/{key}`; any member can
read it. A key is lowercase letters, digits and underscores, starting with a letter. The type
is one of `string`, `number`, `boolean`, `timestamp`, `date` or `string_list` and never
changes: to use another type, archive the field and create a new key. A value that arrives
for a key nobody registered registers it, with the type inferred from the value (never
`timestamp` or `date`), `latest` precedence and `inferred` set. A value that does not fit
the registered type, or that belongs to an archived field, is left out of the record with a
reason; the rest of the record still applies.

**Identifier kinds.** A person is recognised by `email` (trimmed, lowercased), `phone`
(international format only: spaces, dashes, dots and brackets are removed, a leading `00`
becomes `+`, then 8 to 15 digits), `anonymous_id`, or `external:<namespace>` for ids from
another system.

**Merges.** When a record's identifiers match several people, they are merged into the
oldest one: it keeps every identifier and source record and the others point at it. Merging
is refused when the combined identifiers would break a rule. The rules are the same for every
workspace for now (they are not configurable yet): a person holds one value of each
`external:<namespace>` kind, up to 10 emails, up to 10 phone numbers and up to 100
anonymous ids. A refused merge attaches the record to the owner of the strongest kind and
records an **identity conflict** for each other person. Every merge keeps a provenance row
(the identities and source records that moved) and a decision record, and there is no
unmerge. A background job (`person.merge.rewrite`) then re-points what the merge did not
move; reading a merged person follows it to the person it was merged into.

**Conflict review.** Admins work through conflicts under **Conflicts** in the web UI, which
shows the two people side by side with the shared identifier marked and offers two outcomes:
keep the people separate, or move the identifier to one of them (refused when it would break
a rule for that person). Other readers cannot resolve conflicts; identifier values require personal-data access. The API is `GET /api/v1/workspaces/{ws}/identity-conflicts` (filter with
`status=open|resolved`, page with `cursor`), `GET .../identity-conflicts/{id}` and
`POST .../identity-conflicts/{id}/resolve` with `{"outcome": "keep_separate"}` or
`{"outcome": "move_identifier", "to_person_id": "..."}`. Each resolution is audited and
recorded as a decision.

**Precedence.** When several sources supply the same field for one person, the field's
policy picks the value, and the person shows which source it came from:

- `latest`: the most recently observed value wins (the default).
- `source_priority`: the first source in the field's source order that has a value wins;
  sources not listed rank after the list, by `latest`.
- `first`: the earliest observed value wins.

Sending `null` for a field removes that source's value, and the next source's value
(if any) takes over.

Changing a field's precedence, source order or archived flag also re-evaluates the people
who already hold a value for it, through a background job. An archived field is no longer
shown on people; unarchiving brings its values back.

## People

Open **People** in the web UI to find a person and see everything the system knows and did
about them. Any member can look; an API key needs the `read` scope.

**Search.** Search by an identifier, matched exactly after the usual clean-up (an email is
trimmed and lowercased, a phone number put in international form), or by the value of one
field. A person that was merged into another is found as the person they were merged into.
The conditions on a field depend on its type: `eq` and `exists` for any type (on a
`string_list`, `eq` means the list holds the value), `prefix` for text, and `gt`, `gte`, `lt`
and `lte` for numbers, timestamps and dates. The API is `POST /api/v1/workspaces/{ws}/people/search`
with exactly one of `{"identifier": {"kind": "email", "value": "..."}}` and
`{"trait": {"key": "plan", "op": "prefix", "value": "pro"}}`, plus `limit` (1 to 200,
default 50) and `cursor` (the `next_cursor` of the previous page). It is a POST so the values
stay out of URLs and access logs. Results are `{id, display, updated_at}`, paged by person
id, where `display` is the person's strongest identifier. Searches run under a 5 second
limit; one that takes longer is answered `503` with the problem code `search_timeout`, so
narrow it rather than retry it unchanged. A caller who may not see values (see Masking) can search a PII-flagged field only with `eq` and `exists`; `prefix` and the range conditions on it are refused with `403` and the code `pii_trait_not_searchable`. Equality on a field uses an index; the other
conditions scan the workspace's people until the indexing question (OQ-1) is decided.

**Profile.** `GET /api/v1/workspaces/{ws}/people/{id}` returns the identities grouped by the
source whose live records list them (those no record lists, such as one first seen on an
event, come under `observed`), each field with its type, source and the time that source
observed it, the person's consent and lists (the same as their own endpoints), and the people
merged into this one. An id that was merged into another returns the survivor with
`merged_from` set to the id you asked for. The web UI shows the same on the profile page,
where an editor or above can also record a consent choice for the person.

**Timeline.** `GET .../people/{id}/timeline?limit&cursor&kinds&all_decisions` returns the
person's events and the decisions the system made about them, newest first, as entries with
`"type": "event"` or `"type": "decision"`, paged with an opaque `cursor`. Decisions
cover trait changes, merges, consent changes, list membership and identity conflicts. The
people merged into this one are included. `kinds` is `event`, `decision` or both separated by
a comma. The `event.accepted` decision repeats the event entry, so it is left out unless you
pass `all_decisions=true`. Every entry has a `trace_id` that opens in **Traces**
(`GET .../traces/{trace_id}`), where the decisions hold ids and reason codes only.

**Masking.** Identifier values, PII-flagged field values and the PII properties of events
are shown to the owner, workspace admins and `admin` keys, memberships with `view_pii`,
and keys with `pii`. Other readers see `masked: true` and no value (the web UI says "Hidden"). An event with no published schema
cannot say which of its properties are personal, so all of its values are hidden from them.
Search is not masked on the way in: a caller who may search can find a person by a value
they already know.

## Contacts

For a company with no customer system of its own, zirv flow can hold the contacts itself. A
**contact** is a person with a record from the built-in `native` source (stream `contacts`),
identified by your **contact key**: 1 to 256 characters, stored as the identifier
`external:contact_key` and used in URLs. Because a contact is an ordinary person, identity
resolution, precedence, consent, suppression, lists, search and erasure work on it unchanged.
`search` and `bulk` are not valid keys, because they name endpoints.

**The definition.** `GET` and `PUT /api/v1/workspaces/{ws}/contact-definition` read and save
the shape of a workspace's contacts: a list of `{key, required, identifier}`, where `key`
names a field of the [field registry](#people-and-fields), which supplies the type;
`required` makes a contact without it a `422`; and `identifier` (`email`, `phone`,
`anonymous_id` or `external:<name>`) also makes the value an identity of the person, with the
usual normalisation. An identifier field must be a `string` field. Saving needs the workspace
admin role (an `admin` key) and is audited as `contact_definition.updated`; unknown and
archived registry keys come back as a `422` listing each by position. Without a saved
definition a contact needs only its key and accepts any valid registry key; once one is
saved, a contact may hold only the fields it lists. Open **Contacts**, then **Definition**,
in the web UI to edit it.

**Single contacts are synchronous.** Each write is validated against the definition and
applied in the request, in one transaction with its audit entry, so the answer is the stored
contact:

```sh
KEY=zfk_...   # an API key with the write scope
BASE=http://127.0.0.1:8080/api/v1/workspaces/$WS

# create: 201 with the contact, 409 when the key exists
curl -X POST $BASE/contacts -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -H 'idempotency-key: signup-8841' \
  -d '{"key": "c-1001", "fields": {"email": "ada@example.test", "age": 36}}'

# read, list (keyset-paginated by key), search
curl $BASE/contacts/c-1001 -H "authorization: Bearer $KEY"
curl "$BASE/contacts?limit=50&cursor=<next_cursor>" -H "authorization: Bearer $KEY"
curl "$BASE/contacts/search?q=c-10" -H "authorization: Bearer $KEY"

# replace all the fields (200), or a JSON merge patch where null removes a field (200)
curl -X PUT $BASE/contacts/c-1001 -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"fields": {"email": "ada@example.test"}}'
curl -X PATCH $BASE/contacts/c-1001 -H "authorization: Bearer $KEY" -H 'content-type: application/json' \
  -d '{"fields": {"age": 37}}'

# delete: 204
curl -X DELETE $BASE/contacts/c-1001 -H "authorization: Bearer $KEY"
```

A missing required field, a wrong type or a field the definition does not list is a `422`
whose `errors` name the field and a reason code (`missing_required`, `wrong_type`,
`unplanned_property`, `invalid_email`, ...) and never echo a value. Removing a required
field with a patch is refused the same way. The key is also kept as the contact's first
identifier; changing the value of an identifier field (by `PUT`, `PATCH` or a bulk upsert)
replaces the identity: the old one leaves the person unless another source's record of the
person also lists it, so it is no longer found by search and can no longer merge people.
If the definition drops an identifier field and no remaining field produces that identifier
kind, a `PATCH` keeps its stored value and identity until a `PUT` or bulk upsert retracts the
field.

**Search.** `GET .../contacts/search?q=` matches a contact key by prefix and an email address
or phone number exactly, after normalisation. The value is then in the URL and the access
logs, so for addresses and numbers use `POST .../contacts/search` with
`{"identifier": {"kind": "email", "value": "..."}}` or `{"q": "..."}`; the web app does.

**Idempotency.** Send an `Idempotency-Key` header (1 to 255 characters) on a create, replace,
patch or delete to make a retry safe. The same key with the same method, path and body
replays the first status and changes nothing, and the response carries `Idempotent-Replayed:
true`; the body is the contact as it is stored now, or `404` if it has since been deleted or
erased, and a delete replays `204`. The same key with a different request is a `422` with the
code `idempotency_key_reused`. Keys are remembered for 24 hours and then forgotten by the
scheduler leader. The table that remembers them (`contact_idempotency`) holds the key, a
hash of the request and the status, and no field value, person id or contact key, so
normal deletion has no person reference to follow there. Live erasure also checks these keys by identifier value. Without the header an identical repeat of a
replace or patch still writes nothing, because a write that changes nothing is a no-op.

**Bulk upsert is asynchronous.** `POST .../contacts/bulk` takes
`{"contacts": [{"key": "...", "fields": {...}}]}`, at most `ingest.max_batch_items` of them. Every item
is validated first against the definition (or, in a workspace without one, the field registry), as a single write is; one invalid item makes the whole request a `422`
with `errors` by `index`, `field` and `reason`, and nothing is stored. Valid items are appended
to the ingestion inbox as kind `contact` and the answer is `202 {"accepted": n}`. The
`ingest.drain` job applies them in order, replacing the fields of an existing contact (a field the registry has since archived rejects the row), and
the result is the same state the single-contact writes make. A bulk request is limited like
ingestion: the same body size and the same per-key rate, which a signed-in session shares per
user. Use it for imports and use the single endpoints when you need the stored contact in the
answer.

**Permissions and masking.** Reads need the `read` scope and writes the `write` scope (or
an editor or above in the app; a session needs the CSRF token for writes). Values
of identifier fields and of fields flagged personal data are shown to the owner, workspace
admins and `admin` keys, memberships with `view_pii`, and keys with `pii`; other readers get `null` for them and the field names in
`masked_fields`. That applies to the response of a write too.

**Deleting a contact** retracts its `native` record. What stays of the person follows the
workspace's [orphan policy](#deletion-erasure-and-export): another source's data on the same
person stays, and under the `delete` policy a person left with nothing is deleted with the
contact. Live erasure also scans idempotency keys for matching identifier values; the exact matching limits are described below.

## Events

The event store holds the typed events people produce. Each event name has a schema; the
store checks every incoming event against it before it is kept. Events arrive through the
[ingestion API](#ingestion); the schemas, settings and rejects are managed under **Events**
in the web UI.

**Schemas and versions.** A schema lists the properties of one event name: each has a type
(`string`, `number`, `integer`, `boolean`, `timestamp`, `date`, `string_list` or `object`),
may be required, may list allowed values (text only) and may be marked as personal data.
Admins edit a *draft* and *publish* it as the next version; a published version never
changes (the database refuses it), and publishing clears the draft. Until a version is
published the schema is not in force. Admins manage schemas through
`GET` and `POST /api/v1/workspaces/{ws}/event-schemas`, `GET` and `PATCH .../event-schemas/{name}`,
`PUT .../event-schemas/{name}/draft` and `POST .../event-schemas/{name}/versions`; any member
can read them. Event names in paths are percent-encoded.

**Modes.** A schema is in one mode. `allow` accepts an event that breaks it without flagging
it, `warn` accepts and flags it (`validation_state = flagged`), `block` rejects it. The
reasons name the property and the problem (`missing_required`, `wrong_type`, `not_in_enum`,
`unplanned_property`), never a value. Validation sees the values as they were sent, before
redaction.

**Unplanned names.** An event whose name has no published version follows the workspace
policy (`GET` and `PUT .../event-policy`): `draft` (the default) stores it as `unplanned` and
drafts a schema from the observed shape, marked inferred; `reject` refuses it. Inference never
publishes.

**Time.** `occurred_at` is the client timestamp, or the receipt time when there is none. A
timestamp more than `future_skew_seconds` (default 300) ahead of the receipt time is replaced
by it and flagged `clamped`. One more than `lateness_seconds` (default 86400) behind is kept
and flagged `late`, and the person's pending change records the earliest such time
(`late_from`) so time-window segments can be recomputed from it.

**Repeats.** A client `messageId` gives effectively-once storage: only an accepted event
claims it, a repeat is counted and linked to the original rather than stored, and a rejected
message can be sent again once fixed. A message that already has an open reject writes no
second reject.

**Redaction.** Before an event or a reject payload is stored, the policy's rules drop
properties, replace them with a keyed hash (derived from the master key; equal values stay
equal inside one workspace) and truncate IP addresses (the last IPv4 octet is zeroed, an IPv6
address keeps its first 48 bits; `ip` and `context.ip` by default). Paths are dot-separated.

**Rejects.** A refused event is listed under **Rejects** (`GET .../ingest-rejects`, newest
first, filtered by `name` and `reason`, paged with `cursor`) with its reasons and a masked
sample: only the values of properties the published schema marks as not personal data are
shown, and every value of an event without a published schema is hidden. The stored payload
is kept for a later replay and is never returned.

**Retention.** (All windows: [Retention windows](#retention-windows).) Events are kept for `events.retention_days`, or a per-name number of days an
admin sets within `events.min_retention_days` and `events.max_retention_days`; rejects for
`events.reject_retention_days`. A daily job (03:17 UTC) deletes what is past its limit in
batches, drops whole monthly partitions older than the longest limit and removes the repeat
records of events that no longer exist. Each batch and each partition drop commits separately,
with its handled markers, so a failed run keeps completed work and the next run continues. A
partition drop waits at most 2 seconds for the table lock and is skipped (logged, retried by the
next daily run) rather than stalling ingest.
The scheduler leader creates the partitions for this and next month.

**Decision-log retention.** A daily job (03:53 UTC, `decisions.retention`) drops each monthly
decision-log partition that ends before `retention.decision_days`, in a transaction of its own
(never the default partition, this month or next), then deletes the older rows of the month
that holds the cutoff in batches. A drop waits at most 2 seconds for the table lock and is skipped
(logged, retried by the next daily run) rather than stalling appends. A workspace with a shorter
window is deleted in key order with a cursor, so the job reads the other workspaces' rows below
the cutoff once and no batch rescans an earlier one; it adds no index. It sets `zirv_flow_decision_log_bytes` and
`zirv_flow_decision_log_partitions` and counts deletes in
`zirv_flow_retention_deleted_total{table="decision_records"}`. Measured on PostgreSQL
(200,000 records per shape, with the primary key and the trace, person and enrollment
indexes), a record takes about 450 bytes (ingest), 730 (flow step) and 800 (send), and about
260, 430 and 500 without the three secondary indexes. At 1,000 events/s with one record per
event that is about 39 GB (ingest), 63 GB (flow) or 69 GB (send) a day, so the 13-month
default holds about 15 TB (ingest), 25 TB (flow) or 27 TB (send) per record-per-event stream at that rate; scale it by the real
records per event and rate. Reproduce the table with
`ZIRV_FLOW_BENCH_DECISION_SIZE=200000 cargo nextest run -p zirv-flow-store --run-ignored all --no-capture bytes_per_record`.
**Job-history and audit-log retention.** Two daily jobs run at 04:07 UTC (`jobs.retention`) and
04:19 UTC (`audit.retention`). The first deletes dead jobs whose `dead_at` is older than
`retention.job_history_days`; `ready` and `running` jobs are never touched. The second deletes audit
rows older than `retention.audit_days` through the database's `audit_purge` function, the only
delete the append-only guard allows, and appends one `audit.purged` row (system actor, summary
`count` and `cutoff` only) when it deleted anything. Both delete in batches of 5,000 that commit
separately, so a failed run keeps its completed batches and the next run continues; the deletes are
counted in `zirv_flow_retention_deleted_total{table="jobs"}` and `{table="audit_log"}`.

### Retention windows

Every window the deployment enforces, in one place. A window is set by the operator in the
configuration; a workspace admin may only shorten the message and decision-log windows, through
`PUT /api/v1/workspaces/{ws}/retention` (admin only, audited as `retention.updated` with the
window names and day counts). Events are shortened per event name through the event schemas, and
assistant transcripts stay under the AI settings.

| Window | Key | Default | Bounds | Who may change it | Job |
|---|---|---|---|---|---|
| Events | `events.retention_days` | 760 days | per-name overrides 30 to 1095 (`events.min_retention_days`, `events.max_retention_days`) | operator; a workspace admin per event name | `events.retention` |
| Ingest rejects | `events.reject_retention_days` | 30 days | at least 1 | operator | `events.retention` |
| Messages | `retention.messages_days` | 395 days | workspace window 30 to 1095 (`retention.messages_min_days`, `retention.messages_max_days`), never above the deployment window | operator; a workspace admin may shorten it | `messages.retention` |
| Decision log | `retention.decision_days` | 395 days | workspace window 30 to 760 (`retention.decision_min_days`, `retention.decision_max_days`), never above the deployment window | operator; a workspace admin may shorten it | `decisions.retention` |
| Audit log | `retention.audit_days` | 730 days | 90 to 3650 | operator | `audit.retention` |
| Job history | `retention.job_history_days` | 14 days | 1 to 90 | operator | `jobs.retention` |
| Import files | `retention.import_file_days` | 7 days | 1 to 90 | operator | scheduler leader, hourly |
| Import rejects | `retention.import_reject_days` | 30 days | 1 to 365 | operator | scheduler leader, hourly |
| Person exports | `retention.export_hours` | 24 hours | 1 to 168 | operator | `privacy.export` sets the expiry; scheduler leader expires hourly |
| Test sends | `retention.test_send_months` | 13 months | 1 to 36 | operator | `events.retention` |
| Assistant transcripts | `transcript_days` in the AI settings | none stored | 1 to 30 | workspace admin | `maintenance.ai_transcripts` |

`GET /api/v1/workspaces/{ws}/retention` (admin only) lists eight of these as `windows`, each with
`key` (`events`, `messages`, `decision_log`, `audit`, `ingest_rejects`, `import_files`,
`job_history`, `assistant_transcripts`), `value`, `unit`, `source` (`deployment` or `workspace`),
`min`, `max`, `editable` and `last_run` (`ran_at` and `deleted`, from the last run of its job; null
before the first). `PUT` takes `messages_days` and `decision_days`, each a number of days or null
for the deployment value; a value below the minimum or above the deployment window is a 422
(`retention_out_of_bounds`) naming the bounds, and any other field is a 422. A workspace's shorter
window deletes that workspace's rows only.

## Event debugger and explorer

Under **Events** in the web UI, **Debugger**, **Explorer** and **Rejects** show what the event
store did with what was sent.

**Debugger.** A live list of events as they are decided, newest first (`GET
/api/v1/workspaces/{ws}/debugger/stream`, server-sent events, `read` access). Each frame
is an `item` with the outcome (`accepted`, `flagged`, `unplanned`, `rejected` or `duplicate`),
the reasons, the properties and a trace link; filter by source, event name and outcome. The
page can pause and resume, shows a notice when it could not keep up (`event: gap`) and
reconnects from `Last-Event-ID`. It is live: opening it shows nothing from before except the last few seconds. A
workspace has at most 20 streams per server process (more get 429), and the page says when
the server's notifier is down and updates are delayed.
Ingestion writes the feed only while someone watches the workspace (a viewer renews a 30 s
watch every 10 s); with nobody watching it pays nothing extra, and feed rows older than 15
minutes are deleted. The feed is unlogged and holds redacted values only. Frames can arrive out of order: a row whose writer commits late is sent when it does, and the
page shows it once whatever its `seq`. Each frame id is `<epoch>:<position>`; after a database
crash the unlogged feed starts over, and a stream resumed from before it is told with a
`gap` frame whose `reset` is true (the page clears its list and says so). The remaining limit:
a row whose writer commits more than about 20 seconds after the row was written is past the
lookback window and can still be missed. A stream resumed from `Last-Event-ID` does not deliver
a late row below its resume point whose writer committed while the client was away; the
lookback covers live streams only. An open stream also notices a crash and sends the reset.

**Masking and reveal.** Values are masked per viewer as in the person timeline: only
properties the published schema declares non-PII are shown, and every other value is null and
listed as masked. Callers with personal-data access may ask for `reveal=true` (the
**Reveal PII** toggle) to see the stored values. They are the values after ingest redaction,
so dropped and hashed properties stay dropped and hashed. Readers without that grant never see
them.

**Explorer.** `POST /api/v1/workspaces/{ws}/events/explore/count`, `.../breakdown`,
`.../list` and `.../export` (POSTs, so filters stay out of URLs). A query names a time range
of at most 400 days, optionally an event name, a person (merged people included) and
property filters. Operators follow the schema type of the property (`eq`, `ne`, `in`,
`prefix`, `gt`, `gte`, `lt`, `lte`, `exists`); undeclared properties compare as text. The page
draws counts per hour, day or week as an inline SVG bar chart with the same numbers in a table,
a top-50 breakdown of one property, paged rows and a CSV export (audited as `events.exported`
with the shape of the filters and the row count, never values). A caller without access to
values can filter PII and undeclared properties only with `eq` and `exists` and cannot break
them down. Queries always carry the workspace and the time range, run under the `explorer.*`
timeouts, and answer `503` with the code `query_timeout` instead of running unbounded.

**Replay.** After fixing a schema or the settings, an admin can replay rejects from
**Rejects**: select some, or replay everything that matches the filter (at most 10,000). A
preview checks each reject's stored, redacted payload against what is in force now and
predicts the outcome without writing; confirming queues an `ingest.replay` job. An event that
now passes is stored once, flagged `replayed`, with an `event.replayed` decision that records
why. A second replay of the same reject does nothing, and a message id that is already stored
counts as a duplicate. A reject that never named a person cannot be replayed. **Downstream
triggers stay off by default**: replayed events also carry the flag `no_triggers`, which the
flow event-trigger path skips; turn on **Emit downstream triggers** only if
people should still receive what these events would have started.

## Deletion, erasure and export

Three different things happen to a person's data, and they are kept apart on purpose.

**Removing a contribution.** When a source retracts a record, only that source's contribution
goes. The record is tombstoned, the person's traits are recomputed from the other sources, and
each identity that only that record listed is deleted (together with the conflicts about it).
An identity first seen only through an event is removed too, and the next event with it creates
it again. Events linked to that source record are removed with it. Unlinked ingestion and contact events remain. The person is
marked changed. What happens if nothing holds the person any more (no live record, no event and
no explicit consent) is the workspace **orphan policy**: `keep` (the default) or `delete`. Set it
under **Privacy** in the web UI or with `PUT /api/v1/workspaces/{ws}/privacy/settings`
(`{"orphan_policy": "delete"}`; workspace admins, CSRF, audited).

**Deleting a person.** `DELETE /api/v1/workspaces/{ws}/people/{id}` (workspace admins and `admin`
keys; CSRF; audited as `person.deleted` with the id only) deletes the person, the people merged
into them, and every row that refers to them: identities, source records, events in every
partition, dedupe rows, ingest rejects, list memberships and their history, consent and its
history, identity conflicts, merge provenance, pending exports and the person's decision
records. It adds no suppression and no ledger entry, so the same email can create a new person
the next time it is ingested.

**Erasing a person.** Use erasure for a data-subject request. `POST
/api/v1/workspaces/{ws}/privacy/erasures` takes `{"person_id": "..."}` or `{"identifier":
{"kind": "email", "value": "..."}}` (an identifier that matches nobody is a 404) and needs the
master key. It writes a row to the erasure ledger with the keyed hash of every identity of the
person and of the people merged into them (never a value) and enqueues `privacy.erase`. The job
runs in one transaction: it collects the person's group and identities as they are when it runs, adds
their hashes (and the group's ids) to the ledger row, suppresses every email and phone hash as
`erased`, deletes the workspace's undrained ingestion inbox rows with an identifier that hashes to any
of them, so a row drained later cannot create the person again (an `erased` suppression overrides any other reason and can never be removed); it deletes the group exactly as above; it completes the ledger row; and it audits `privacy.erased`
with the request id. A failure rolls everything back and the job is retried; a request whose job
ran out of attempts shows as `failed`. `GET .../privacy/erasures` lists the requests, newest
first, each with the erase call made to every destination that declares erase and handled the person
(`destination_calls`: outcome `pending`, `done` or `failed`, attempts and a fixed error code; admins
only). The `destination.erase` job makes those calls after the erasure commits, once per
destination, with an idempotency key that is stable across retries; a failing destination is retried
and then shown as `failed`, and never blocks the erasure. The ledger is the one place that keeps the
person id of an erased person, by design.

The logs hold no identifier values: no email, phone, external id, name or trait value, in the
message or in a span field, and no keyed hash of one. The end-to-end test
`default_level_logs_hold_no_identifier_value_through_the_whole_privacy_scenario`
(`crates/zirv-flow/src/e2e/privacy.rs`) captures the real JSON log layer at the default `info`
level through ingest (including a refused batch and a rejected event), a profile read, 4xx and 5xx
answers, an export and its download, a failing send, an erasure whose destination fails its erase
call, and a replay. It then scans every line for each seeded value raw, lowercase, percent-encoded
and as a prefix of its identifier hash. A control line proves the scan can fail.

Erasure removes what was held or pending when it ran. Data that arrives while it runs, or after it,
creates a new person, who stays suppressed: the suppression keeps the address from being contacted.

**CSV imports.** Import files and refused rows have no person foreign key. Live erasure
scans their content for identifier values, with the matching limits below; normal deletion
does not. Unmatched content ages out under `retention.import_file_days` (default 7 days
after the import ends) and `retention.import_reject_days` (default 30 days from rejection). At the defaults both age out within the one-month window of
an erasure request; a longer configured window must stay within it; see [CSV import](guide/sources.md#csv-import).

**After a restore.** Backups keep erased data until they age out, so a restore is not finished
until `zirv-flow admin replay-erasures --from <ledger_dir>` has run and before any role starts. The
steps, in order, with a check for each, are in the
[backup and restore guide](operations/backup-restore.md); the drill that proves them is
`scripts/drill/restore.sh`, with its log in
[docs/operations/evidence/restore-drill.md](operations/evidence/restore-drill.md).

<a id="privacy-ledger-dir"></a>**The ledger outside the database.** A backup taken before an
erasure holds neither the deletion nor the ledger row, so the replay above has nothing to replay.
Set `privacy.ledger_dir` (or `ZIRV_FLOW_PRIVACY__LEDGER_DIR`) and the `privacy.erase` job also
writes one JSON file per completed ledger entry, `<workspace_id>-<ledger_id>.json`
(`zirv-flow.erasure-ledger/v1`), before the job returns: it writes a temporary file in the same
directory, fsyncs it, renames it into place and fsyncs the directory. If the write fails the job
fails, the erasure rolls back and the request stays pending for the retry, so an erasure is never
complete without its file. A file holds ids, keyed hashes (hex), their kinds and times, never an
identifier value. Operator duties:

- Put the directory on storage shared by every worker that supports atomic rename and durable
  writes, owned by the user the workers run as and mode `0700`; the files are created `0600` on
  unix. At start-up a worker writes, fsyncs, renames and removes a probe file there and refuses to
  start, naming `privacy.ledger_dir` and the path, if that fails.
- Back the directory up apart from the database, so one restore cannot take both back in time.
  The hashes are keyed with the deployment HMAC root, so restoring from the files also needs the
  master key and the database's `deployment_keys` row.
- After restoring a database, run `zirv-flow admin replay-erasures --from <dir>` before it serves
  traffic or any worker starts. It reads each file on its own (a corrupt or unknown-format file
  is named on stderr and makes the command exit non-zero, but the other files are still
  applied), adds the entries the database lacks as
  completed, upgrades a request the backup still holds as pending or failed to completed (the file
  records a completed erasure and wins; a completed row is left alone), skips and counts files
  whose workspace does not exist, and then runs the replay. The counts include `merged` (added
  and upgraded) and `skipped_workspaces`. A file whose ledger id belongs to a request of another
  workspace in the database stops the merge before any change, naming the file. A request merged
  or upgraded from a file runs only the replay: it does not make the destination erase calls and
  does not erase AI transcripts, which is the existing limit of replay.
- A hung `privacy.ledger_dir` stalls the erase jobs, and the row locks they hold, until the
  file system returns, so the directory must be reliable storage.
- If a retry of an erasure finds its file already there with other content (the person gained an
  identity since the first attempt, or a pending request came back from an older backup), the job
  writes the union of the two to the file and to the ledger row; the file never shrinks. A file that
  is unreadable or invalid fails the job, which the worker logs naming the file. Both steps are safe to run again.
- With the directory set, the recovery point for erasures is none: the file exists before the
  erasure is reported complete. With it unset, nothing is written and the restore guarantee rests
  on backups of the database alone.
- `zirv-flow admin export-ledger --out <dir>` writes the same files for every completed entry in
  the database, a snapshot that also covers erasures made before the key was set. It prints counts
  and writes no audit entry.
- The replay re-erases from the ledger's ids and hashes only. Data that names neither a ledger
  person id nor a ledger hash is out of its reach: transcripts, rejects that never named a
  person, imported files and refused rows, and idempotency keys are found by value, so the live
  erase is the control and their configured retention windows (`retention.import_reject_days`, `retention.import_file_days`,
  `retention.export_hours`; defaults 30 days, 7 days, 24 hours) bound the rest.
- **What the live erase finds by value, and its limits.** The person's ids and identifier values
  are matched ignoring case. Assistant transcripts keep substring matching. Everything else
  matches an identifier only as a whole field: a CSV cell (bounded by the start or end of the
  text, a newline, a quote, or one of `,` `;` tab `|`) in the stored import files and the refused
  rows' cells, a JSON scalar value, the message id or the sample cell in rejects with no person,
  and the whole contact idempotency key. An email also matches as a substring anywhere in those,
  because it is distinctive and is the common value embedded in a notes column. So an external id
  `42` deletes a `42` cell and never `1427`; a non-email identifier embedded in free text is not
  found, and a value spelled differently is missed. A value split across two chunks is found, and
  the whole file is deleted; every such import that has not ended is set `failed` with the code
  `erased` in the same transaction, so a running worker cannot commit a batch afterwards. Cost:
  the erase reads the workspace's import files within the `retention.import_file_days` window (default 7 days) once, each chunk tested against
  all identifiers in one pass. Each delete is bounded by the workspace. The content scan also reads
  the surviving tables (suppressions, the ledger, the audit log): their rows stay, plaintext may
  not. The erasure ledger file (`privacy.ledger_dir`) is outside the database, so the scan does
  not cover it.

**Exporting a person.** `POST .../privacy/exports` (same body as an erasure; workspace admins
only; CSRF; audited as `privacy.export_requested`) enqueues `privacy.export`, which builds one
gzip-compressed JSON document, `zirv-flow.person-export/v1`: `person` holds `profile` (traits
with their sources), `identities`, `source_records`, `events` (every partition, oldest first),
`consent`, `consent_history`, `lists`, `segments`, `list_history`, `merges` and
`identity_conflicts`, then the flow and send side: `enrollments`, `step_executions`,
`enrollment_waits`, `flow_entries`, `messages` and `message_links` (never the recipient hash or a
rendered body), `forwarding_outbox`, `erasure_handled_markers`, `ingest_rejects` (the redacted
payload; rows with no person are not reachable), `decision_records` and
`template_preview_people`, with the data of merged people under the survivor. Left out on
purpose, each checked by a test against every table with a person column: the change feed
(`person_changes`), the export itself (`privacy_exports`), `event_dedupe`, the debugger buffer,
`jobs` (copies of data already exported), the assistant's stored transcripts (free text found by
needle on erase only, so an export cannot find it) and the suppression and erasure ledgers. The
document may hold at most `privacy.export_max_bytes` of JSON before compression (default 256 MiB);
a person over the cap ends as `failed` with the code `export_too_large` and nothing is stored.
It is the one place values leave the system, so only
workspace admins can list exports or download one, and every download is audited as
`privacy.export_downloaded`. `GET .../privacy/exports/{id}/download` returns the file for the configured window
(`retention.export_hours`, default 24 hours) after the export is ready and then answers 410; the scheduler leader drops expired content
hourly. Erasing the person deletes their exports.

Everything above is on the **Privacy** page of the web UI, visible to workspace admins and the
owner. Erasing asks you to type `ERASE` first. Erasure reaches only destinations that declare an
erase call and does not rewrite files outside the database.

## Ingestion

Applications send profile updates and events over HTTP with a write-only API key. There are
no client SDKs; the API is three JSON endpoints.

**Create a key.** A workspace admin creates a key whose only scope is `ingest`
(`POST /api/v1/workspaces/{ws}/api-keys` with `{"name": "web app", "scopes": ["ingest"]}`).
The response shows the `zfk_...` key once. The key implies its workspace. A key without the
`ingest` scope, and a signed-in session, get 403; a missing or invalid key gets 401. Ingestion
needs no CSRF token.

| Endpoint | Body |
|----------|------|
| `POST /api/v1/ingest/identify` | one profile update |
| `POST /api/v1/ingest/track` | one event |
| `POST /api/v1/ingest/batch` | `{"batch": [item, ...]}`, each item an identify or a track with `"type": "identify"` or `"type": "track"` |

```sh
curl -X POST https://flow.example.com/api/v1/ingest/identify \
  -H "Authorization: Bearer $ZIRV_FLOW_KEY" -H "Content-Type: application/json" \
  -d '{"userId": "u-1042", "identifiers": {"email": "sam@example.com"},
       "traits": {"plan": "pro"}, "messageId": "signup-u-1042"}'

curl -X POST https://flow.example.com/api/v1/ingest/track \
  -H "Authorization: Bearer $ZIRV_FLOW_KEY" -H "Content-Type: application/json" \
  -d '{"userId": "u-1042", "event": "order_placed", "properties": {"total": 59.9},
       "timestamp": "2026-03-01T09:30:00Z", "messageId": "order-7731"}'

curl -X POST https://flow.example.com/api/v1/ingest/batch \
  -H "Authorization: Bearer $ZIRV_FLOW_KEY" -H "Content-Type: application/json" \
  -d '{"batch": [
        {"type": "identify", "userId": "u-1042", "traits": {"plan": "team"}},
        {"type": "track", "userId": "u-1042", "event": "plan_changed", "messageId": "chg-55"}]}'
```

```js
const response = await fetch("https://flow.example.com/api/v1/ingest/track", {
  method: "POST",
  headers: {
    Authorization: `Bearer ${process.env.ZIRV_FLOW_KEY}`,
    "Content-Type": "application/json",
  },
  body: JSON.stringify({
    anonymousId: "a-77",
    event: "page_viewed",
    properties: { path: "/pricing" },
    messageId: crypto.randomUUID(),
  }),
});
if (response.status === 429) {
  const seconds = Number(response.headers.get("Retry-After")); // wait, then resend the same body
}
```

**Fields.** Every item takes the following; any other top-level field is refused with
`unknown_field`, never dropped.

| Field | Meaning |
|-------|---------|
| `userId` | your id for the person; stored as the identifier `external:user_id` |
| `anonymousId` | stored as the identifier `anonymous_id` |
| `identifiers` | an object of identifier kind to value: `email`, `phone`, `anonymous_id` or `external:<namespace>`, such as `{"email": "...", "external:crm": "..."}` |
| `timestamp` | RFC 3339; when it happened. The arrival time when absent |
| `messageId` | 1 to 200 characters; the retry key |
| `traits` | identify only: an object of trait key to value; `null` removes the trait this source set |
| `event`, `properties` | track only: the event name (required, 1 to 128 characters) and an object |

At least one of `userId`, `anonymousId` and `identifiers` is required. Identifiers are checked
and normalised as everywhere else (an e-mail is lower-cased, a phone number is E.164). The
request is checked for structure only; whether an event name is planned or its properties fit
its schema is decided when the event is applied, and a refused event appears under **Rejects**.
If any item is invalid the whole request is a 400 and nothing is stored; the problem body lists
`errors: [{index, field, reason}]` with field names and reason codes, never values.

**Limits.** A body over `ingest.max_request_bytes` is a 413 and more than `ingest.max_batch_items`
items is a 400 (`too_many_items`). Each key has a token bucket of `ingest.burst_per_key` items
refilled at `ingest.rate_per_key` per second, counted in items, in each `serve` process; with
several, a key may send each its own share. A request that does not fit is a 429 with a
`Retry-After` header in whole seconds (at least 1): wait that long and resend it unchanged.
When PostgreSQL is unavailable the answer is 503 and nothing was stored; there is no local
buffer, so retry.

**What 202 means.** `{"accepted": n}` is returned after the items have been appended to a
durable inbox in PostgreSQL. It means durable, not applied: workers, woken at once, apply the
inbox in order, normally within moments. The request's trace id (the `x-trace-id` response
header) is carried into the decisions the items cause, so
`GET /api/v1/workspaces/{ws}/traces/{trace_id}` shows what became of them.

**Retries.** Send the same `messageId` when you resend an item: an event with a `messageId`
that was already stored is counted as a repeat and not stored twice, and a repeated identify
changes nothing. An item without a `messageId` is given a server-generated one when it is
appended, so a retry of the same request by the client without its own `messageId` can store
an event twice. An item that cannot be applied at all becomes an `apply_failed` reject instead
of blocking the workspace's inbox.

**Operation.** The `ingest.drain` job applies up to `ingest.drain_batch_size` inbox rows per
transaction, in arrival order, and deletes them in that transaction. Rows are bucketed by
record key (the strongest identifier) when they are appended, and the job runs
`ingest.drain_shards` loops at once, one per share of the buckets, so a single workspace is
not bound by one transaction's round trips; the rows of one record key always land in one
loop and still apply in arrival order. A batch is up to `ingest.drain_batch_size` of the oldest rows of the
shard's buckets. Legacy rows without a bucket are bucketed first so they join their key's queue by id;
rolling upgrades are not supported. The worker's pools together allow
`(2 + ingest.drain_shards) * worker.concurrency + 1` connections (41 at the defaults),
in addition to the main database pool; see [Background jobs](operations/runtime.md#background-jobs). Concurrent workers never apply a row twice. The
scheduler leader enqueues a drain for any workspace whose oldest row has waited more than 2 seconds, which covers an append that lands as a drain finishes. The
inbox holds the raw item only until it is applied. Metrics: a `serve` process exports
`zirv_flow_ingest_requests_total{endpoint,status}` and `zirv_flow_ingest_items_total{kind}`;
a `worker` process exports `zirv_flow_ingest_applied_total{kind,outcome}` (`valid`, `flagged`,
`unplanned`, `rejected`, `duplicate`, `apply_failed`) and `zirv_flow_ingest_late_total`; the
scheduler leader exports `zirv_flow_ingest_inbox_age_seconds`, the age of the oldest row.

