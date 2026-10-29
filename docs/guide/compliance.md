# Compliance guide

This page is for a data protection lead and for the marketer who runs the campaigns. It separates
what Zirv Flow enforces in code from what stays your duty. It is not legal advice, and nothing here
claims that Zirv Flow, or a deployment of it, is certified or compliant with any law or standard.

Part 1 lists what the software enforces, with each behavior's limit. Part 2 lists what only you can
do. Every enforced row has an id (E1, E2, ...) and each id is tied to a test that exists in the
code, in the [claim-to-test map](compliance-tests.md). A check in the test suite fails when a route,
a configuration key, a metric, a link or a test named on these pages does not exist.

## The matrix

| Requirement | Enforced | Operator duty | Where |
| --- | --- | --- | --- |
| Consent before marketing mail | E1 consent policy with rules that deny a send; E2 consent per person, channel and topic, with an append-only history | Collect consent with clear wording and keep proof (duty 2) | [Consent](#consent-and-unsubscribe), [Reference](channels.md#consent-and-suppressions) |
| Easy opt-out | E3 a signed unsubscribe link and a preference page in every marketing message | Test the link in the mail clients you care about | [Consent](#consent-and-unsubscribe), [Reference](channels.md#tracking-and-unsubscribe-links) |
| Never mail an address again | E4 suppressions by keyed hash, per workspace and organization-wide | Import the suppressions you already hold elsewhere | [Suppressions](#suppressions) |
| Stop a send that should not go | E5 send gates, each recorded in the trace; E6 quiet hours and frequency caps | Choose the caps and quiet hours that fit your promises | [Send gates](#send-gates-quiet-hours-and-frequency-caps), [architecture 10.1](../architecture.md#101-destinations-and-the-send-pipeline) |
| Sender authentication | E7 a message is skipped unless its sending domain verified SPF, DKIM and DMARC | Publish the DNS records and look after sender reputation (duty 4) | [Sending domains](channels.md#sending-domains) |
| Right to erasure | E8 erasure across every table; E9 merged identities; E10 erase calls to destinations that support them | Handle systems the product does not call (duty 12) | [Erasure](#erasure) |
| Erasure survives a restore | E11 the ledger directory outside the database; E12 replay after a restore | Back up the ledger directory apart from the database and replay after a restore (duties 5 and 7) | [Erasure](#erasure), [backup and restore](../operations/backup-restore.md) |
| Right of access | E13 export of everything held on a person | Answer requests that reach you outside the product (duty 9) | [Export](#export-of-a-person) |
| Keep data no longer than needed | E14 retention windows enforced by scheduled jobs | Choose windows within the deployment bounds (duty 11) | [Retention](#retention), [Reference](../people.md#retention-windows) |
| Limit who sees personal data | E15 masking unless `view_pii` or the `pii` key scope | Review members and keys (duty 10) | [Masking](#personal-data-masking) |
| Accountability | E16 append-only audit log with ids only | Review the audit and keep what you need beyond 2 years | [Audit](#audit) |
| Protect credentials | E17 secrets sealed at rest under a master key | Keep and escrow the master key (duty 6) | [Secrets](#secrets-egress-logs-and-the-assistant), [secrets guide](../operations/secrets.md) |
| Limit outbound reach | E18 egress policy for outbound HTTP | List only the internal hosts you mean to reach | [Secrets](#secrets-egress-logs-and-the-assistant), [Reference](sources.md#egress) |
| No identifiers in logs | E19 default-level logs hold no identifier value | Protect and age the logs you ship elsewhere | [Secrets](#secrets-egress-logs-and-the-assistant), [Reference](../operations/runtime.md#logs-and-traces) |
| Control what an AI provider sees | E20 the assistant is off by default and sends no personal data unless an admin opts in | Sign the agreement with the provider and decide on the opt-in (duty 3) | [Secrets](#secrets-egress-logs-and-the-assistant), [Reference](templates.md#ai-assistant) |

## Part 1: What Zirv Flow enforces

### Consent and unsubscribe

**E1. Consent policy.** A workspace has one consent policy: an ordered list of named rules that can
only deny a marketing send. The built-in rule `recorded_consent` denies when the recorded consent
does not allow the send; it can be moved but not removed. The first rule that matches denies the
send and is named on the message. An editor saves the draft, and an admin publishes it with
`POST /api/v1/workspaces/{ws}/consent/policy/publish`. A published version cannot be changed, and
each publish is recorded in the decision log. A rule that no longer compiles denies the send.
*Limit:* a transactional message skips the consent gate (see E5), so keep marketing content out of
transactional templates.

**E2. Consent records.** Consent is kept per person, channel and topic. The only channel is email.
A state is `subscribed`, `unsubscribed`, `pending_confirmation` or none. A channel-wide
`unsubscribed` blocks every topic. An opt-in topic needs `subscribed`; an opt-out topic reaches
everyone who has not unsubscribed. Every change is appended to a history with its source, the user
behind it, the time and free-form evidence of up to 4096 bytes. Writing the status a person already
has leaves no history row. Read it with `GET /api/v1/workspaces/{ws}/people/{id}/consent/history`.
Merging people keeps the most recent explicit decision, and `unsubscribed` wins a tie.
*Limit:* the history cannot be edited, but erasing the person deletes it. The product records what
you tell it; it does not check that the person really agreed (duty 2).

**E3. Unsubscribe link and preference page.** Marketing mail is sent with `{{ system.unsubscribe_url }}`
(`/u/{token}`) and a `List-Unsubscribe` header with one-click support. A `GET /u/{token}` shows a
confirmation and changes nothing; the `POST /u/{token}` unsubscribes the scope of that message
(its topic and verticals), or the whole channel when that scope no longer exists. The page at
`/u/{token}/preferences` lets the person choose topics. Neither route needs a sign-in; the signed
token authorizes the one message. A bad token shows a not-found page. After a successful unsubscribe, the next marketing
send in that consent scope is skipped for lack of consent. Transactional sends skip the consent gate,
and sends in other topics or verticals can still go out, unless the unsubscribe fell back to the
whole channel. *Limit:* `server.public_url` may be `http` or `https`, but the
one-click header is only useful over `https`, so set an `https` URL. The tokens die with the master
key (see E17).

### Suppressions

**E4. Suppressions.** An email address or phone number can be suppressed whatever anyone consented
to. The reasons are `hard_bounce`, `complaint`, `manual` and `erased`. Only a keyed hash is stored,
never the value, so a suppression survives the erasure of the person it came from. The send gate
checks the hash in two scopes, the workspace and the whole organization, for marketing and for
transactional mail. A hard bounce suppresses organization-wide, so every workspace of the deployment
honors it; a complaint or a manual entry stays in its workspace. An `erased` suppression replaces any
other reason and cannot be removed. Admins manage them with `POST /api/v1/workspaces/{ws}/suppressions`,
`POST /api/v1/workspaces/{ws}/suppressions/remove` and
`POST /api/v1/workspaces/{ws}/suppressions/import`. Lists show only a short prefix of the hash.
*Limit:* suppression needs the master key; without it the endpoints answer 503.

### Send gates, quiet hours and frequency caps

**E5. Send gates.** Before a message leaves, `send.gate` decisions record each check in the trace:
the sender, the address, consent, suppression, the frequency cap and quiet hours. A gate that stops
a message ends it as skipped with a reason (`sender_unverified`, `no_address`, `no_consent`,
`suppressed`, `frequency_cap`) or, for quiet hours, holds it. Consent and suppression name the
deciding rule. A transactional template skips the consent gate, the caps and quiet hours, but not
the suppression gate. The trace shows every gate of every message at `GET /api/v1/workspaces/{ws}/traces/{trace_id}`.

**E6. Quiet hours and frequency caps.** An admin sets them with
`PUT /api/v1/workspaces/{ws}/sending/settings`. A cap is a number of messages per person in a window;
a message over it is skipped or, in defer mode, held until a slot frees. A reservation under a
per-person lock keeps concurrent flows from exceeding a cap. Quiet hours are read in the recipient's
time zone: the person's `timezone` trait, else the workspace zone (the decision notes the fallback).
A message inside a quiet period waits for its end. *Limit:* caps and quiet hours apply to email
marketing only, and a transactional message is exempt from both.

**E7. Sender verification.** A message is skipped as `sender_unverified` unless its template names a
live sender identity whose sending domain is verified. A domain is verified only while its DKIM, SPF
and DMARC records all resolve as expected, and it drops back when a recheck finds one missing.
*Limit:* this proves the records exist, not that your mail reaches inboxes. The DNS lookup uses the
system resolver, outside the egress policy. Reputation is yours (duty 4).

### Erasure

**E8. Erasure of a person.** An admin posts to `POST /api/v1/workspaces/{ws}/privacy/erasures` (the
master key is needed). The `privacy.erase` job runs in one transaction and
removes the person's identities, source records, events in all partitions, ingest rejects, list and
segment memberships, consent and its history, enrollments and their steps and waits, flow entries,
messages, test-send entries, decision records, stored assistant transcripts, pending and dead
forwarding payloads, pending exports and saved preview people. It then deletes the person. A failure
rolls everything back and the request retries. What remains, by design and nothing else:

- the keyed hash of each email and phone as an `erased` suppression (E4);
- the ledger entry: person id, keyed hashes and times, never a value;
- the outcome rows of the erase calls to destinations (E10): destination id, outcome and a fixed
  error code, keyed to the ledger entry;
- audit rows, which hold opaque ids only. They are references, not anonymous data, and erasure does
  not rewrite them.

A scan test reads every text, JSON and byte column of every table for planted values and fails on any
leftover. Data with no person column (rejects with no person, import files, import reject cells,
idempotency keys) is deleted by value in the same transaction.
*Limits:* erasure removes what was held or pending when it ran. Data that arrives later makes a new
person, who stays suppressed. For the data found by value, an identifier matches only as a whole
field, ignoring case, in rejects with no person, import files, import reject cells and idempotency
keys; an email also matches as a substring there, so a non-email identifier inside free text in those
places is not found. Assistant transcripts are different: they are matched by case-insensitive
substring on the person's id and identifier values, within that workspace only. A short identifier
can therefore remove another person's transcript, and a spelling that is not among the person's
identifiers is missed. A changed spelling is missed everywhere. The retention windows bound what
the by-value search cannot reach. Deleting a person (`DELETE /api/v1/workspaces/{ws}/people/{id}`)
is not erasure: it adds no suppression and no ledger entry.

**E9. Merged identities.** The erasure covers the survivor and every person merged into them, with all
their identities. The ledger keeps the ids the group had, so a backup taken before a merge is
erased too.

**E10. Destinations.** Once the erasure commits, the `destination.erase` job calls `erase` on every
forwarding, sync or delivery destination that declares erase support and handled the person, once per
destination, with an idempotency key that is stable across retries. A failing destination is retried,
then shown as `failed`, and never blocks the erasure; read the outcomes with
`GET /api/v1/workspaces/{ws}/privacy/erasures`. *Limit:* webhook and SMTP destinations declare no
erase support and receive no call. A provider destination declares it only when its manifest does.

**E11. The ledger outside the database.** With `privacy.ledger_dir` set, the `privacy.erase` job also
writes one JSON file per completed entry (a temporary file, fsync, rename, fsync of the directory)
before the job returns. If the write fails the job fails and the erasure rolls back, so an erasure is
never complete without its file. A file holds ids and keyed hashes only. At start a worker probes the
directory. `zirv-flow admin export-ledger` writes the entries made before the setting existed.
*Limit:* without `privacy.ledger_dir` a restored backup that predates an erasure has nothing to
replay. The content scan does not cover the files.

**E12. Replay after a restore.** Backups keep erased data until they age out. After a restore,
`zirv-flow admin replay-erasures --from <ledger_dir>` merges the missing entries and erases those
people again, restoring their suppressions, before any role starts. Running it twice changes
nothing. *Limit:* the replay holds only hashes, so it cannot reach data found by value
(transcripts, import files, rejects with no person); their retention windows bound that data. The
steps are in the [backup and restore guide](../operations/backup-restore.md).

### Export of a person

**E13. Export.** An admin requests `POST /api/v1/workspaces/{ws}/privacy/exports`. The
`privacy.export` job builds `zirv-flow.person-export/v1`, a gzip JSON document behind an expiring
download (`retention.export_hours`, default 24). It covers the profile, identities, source records,
events, consent and its history, lists, segments and their history, merges, identity conflicts,
enrollments and their steps and waits, flow entries, messages (without the recipient hash), links,
the forwarding outbox, handled markers, ingest rejects, decision records and saved preview people,
with merged people folded under the survivor. A catalog test fails when a table with a person column
is neither exported nor excluded with a reason. *Limits:* it leaves out internal change feeds, the
debugger buffer, job payloads, dedupe records, assistant transcripts (free text, found by value on
erasure only), and the suppressions and the ledger, which are kept on purpose. The uncompressed
document is capped by `privacy.export_max_bytes` (default 256 MiB); a person over the cap fails with
`export_too_large` and nothing is stored. Each request and each download is audited.

### Retention

**E14. Retention windows.** Scheduled jobs delete data past its window. The deployment sets every
window in the configuration, within fixed bounds, and refuses to start outside them.

| Window | Default | Bounds | A workspace admin may shorten it |
| --- | --- | --- | --- |
| Events (`events.retention_days`) | 760 days | per-name overrides within `events.min_retention_days` and `events.max_retention_days` (30 to 1095) | per event name, within those bounds |
| Messages (`retention.messages_days`) | 395 days | workspace window 30 to 1095, never above the deployment window | yes |
| Decision log (`retention.decision_days`) | 395 days (13 months) | 30 days to 760 days (25 months) | yes, never above the deployment window |
| Ingest rejects (`events.reject_retention_days`) | 30 days | at least 1 | no |
| Import files (`retention.import_file_days`) | 7 days | 1 to 90 | no |
| Import rejects (`retention.import_reject_days`) | 30 days | 1 to 365 | no |
| Job history (`retention.job_history_days`) | 14 days | 1 to 90 | no |
| Audit log (`retention.audit_days`) | 730 days (2 years) | 90 to 3650 | no |
| Person exports (`retention.export_hours`) | 24 hours | 1 to 168 | no |
| Test sends (`retention.test_send_months`) | 13 months | 1 to 36 | no |
| Assistant transcripts | none stored | 1 to 30 days when an admin opts in | yes, through the AI settings |

An admin reads the effective windows with `GET /api/v1/workspaces/{ws}/retention`, which lists
eight: events, messages, the decision log, audit, ingest rejects, import files, job history and
assistant transcripts. Import rejects, person exports and test sends are set only in the
configuration and are not in that list. An admin shortens messages and the decision log with `PUT /api/v1/workspaces/{ws}/retention`; a value outside the bounds
is a 422, every change is audited, and a shorter window deletes that workspace's rows only. The
decision log is dropped by whole monthly partitions, with no hard volume cap. Each run adds to
`zirv_flow_retention_deleted_total`. The audit purge is recorded by a purge entry only when the run deleted something.
*Limits:* a window counts from when a row was written. Consent
records and the erasure ledger have no window; they stay until erasure or deletion.

### Personal data masking

**E15. Masking.** Without access to values, identifiers, PII-flagged traits, consent evidence and
event properties are shown as masked. Who sees values:

- the instance owner and workspace admins;
- a publisher, editor or viewer whose membership has the `view_pii` grant, which an admin sets with
  `PUT /api/v1/workspaces/{ws}/members/{user_id}` and which defaults to off;
- an API key with the `admin` scope, or with the `pii` scope. `pii` alone grants no read access.

The people, list, segment, consent, explorer and import views mask for everyone else. The debugger
shows stored values only when the viewer may see them and asks for `reveal=true`. The values it shows
are the redacted ones: a dropped or hashed property stays dropped or hashed. A rejected event's
sample is cut to the properties the published schema declares non-PII. A real-person template
preview or a flow test that shows personal data needs the grant for an editor.
*Limit:* masking is display only. Data sent to destinations, exports and event forwards is not masked.

### Audit

**E16. Audit.** The audit log is append-only: the database refuses updates, deletes and truncates, and
the only deleter is the retention job. A row holds the actor, the action, the object, the time, the
request id and a summary of ids and counts; a summary with an email, a phone number, a token or a
banned key is refused at write time. It covers sign-ins, membership and key changes, credential
changes, publishes, imports, exports, erasures, retention and configuration; a test calls every
route that changes data and checks it left exactly one row. Workspace admins read their workspace's
rows with `GET /api/v1/workspaces/{ws}/audit`; the instance rows (sign-ins and other rows with no
workspace) are for the owner only, at `GET /api/v1/audit`. Both export as newline-delimited JSON or CSV,
and the export is itself audited. Rows older than `retention.audit_days` (2 years) are purged and the
purge is recorded. *Limit:* reading the audit is not audited, only exporting it; the log records
that a person's data was handled by id, and ids are references, not anonymous data.

### Secrets, egress, logs and the assistant

**E17. Secrets at rest.** Connector credentials, OAuth tokens, AI provider keys, webhook and signing
secrets, DKIM private keys and test-send addresses are stored sealed: each has its own data key
(XChaCha20-Poly1305), wrapped by the master key. A database dump without the key shows ciphertext.
The API never returns a stored connector, provider or signing secret; the exceptions are secrets
you must be given once: a new API key (shown once when you create it), an inbound webhook's shared
secret (shown once on create and on rotate) and a flow's webhook signing secret (shown once on create
and on rotate). The master key also roots the keyed hashes of suppressions, the
ledger and the tracking tokens, so losing it kills those too. *Limit:* other application data is not encrypted at rest by the
application; encrypt the disk and the backups yourself (duty 6). Local passwords are stored as
one-way Argon2id hashes, which cannot be reversed.

**E18. Egress policy.** Outbound HTTP for sources, destinations, webhooks and the assistant goes through
a guarded client that refuses loopback, private, link-local, carrier-grade NAT, cloud metadata and
other special addresses, whether in the URL, in a resolved name or in a redirect. A name that resolves
to a mix of allowed and blocked addresses is refused whole. A cloud metadata address is unlocked
only by an `egress.allow` entry naming exactly that address. *Limit:* sign-in with an OIDC provider and
the sending-domain DNS check are not covered; the operator configures both.

**E19. Logs.** Default-level (`info`) logs hold no email, phone, external id, name or trait value and
no keyed hash of one, across ingest, reads, errors, exports, failing sends, erasure and replay.
Job payloads are never logged. *Limit:* the test covers the default level; do not raise the level in
production without checking what you then collect.

**E20. The assistant's privacy modes.** The assistant is off until the operator lists a provider and an
admin turns it on for a workspace. In the default mode a request carries names, types, descriptions
and counts; a request that would carry personal values is refused. Static header and query values of
a saved manifest are never sent, and webhook URLs are masked in both modes. Two kinds of content go
out in both modes: the template assistant sends the template's authored copy (html, text and
blocks), and only the literals in `visible_if` conditions are masked. An admin can turn on *Allow
personal data in requests*; saved definitions then go out with their literals, and the mapping
assistant sends up to five sample records of a source that it reads for the model. It adds the records in order and
stops at the first one that would take the total past 8 KiB, keeping the earlier ones. They are always fenced and labeled as data. With the opt-in off it sends none. The template assistant
sends no person in either mode. The provider key is sealed (E17). No transcript is stored
unless an admin opts in to 1 to 30 days; an hourly job deletes expired ones and erasing a person
deletes the transcripts that name them (E8). The call log never holds a prompt or an answer.
*Limits:* template text you ask the assistant to edit is sent to the provider in both modes, so keep
personal data out of template text. With the opt-in on, personal data does leave for the provider you
chose.

## Part 2: The operator's duty

Each item is one action. The product helps with some of them, but none of them is done by software.

1. **Lawful basis and records of processing.** Write down, for each workspace, why you process the
   data and what you hold, and keep the record where your data protection lead can find it.
2. **Consent wording and proof.** Put the sign-up wording and the date and place of consent on every
   form, and store it as evidence (the `evidence` field of the consent change) when you record or import
   consent.
3. **Agreements with destinations and AI providers.** Sign a data processing agreement with each
   destination provider and each AI provider before you enable it.
4. **Sender authentication and reputation.** Publish the SPF, DKIM and DMARC records the sending-domain
   page shows, then watch bounce and complaint rates and pause sending when they rise.
5. **Backups that age out erased data.** Set a backup expiry that ages erased people out, and run
   `zirv-flow admin replay-erasures --from <ledger_dir>` after every restore before any role starts.
6. **Key custody and escrow.** Keep the master key apart from the database backups, in a secret
   manager or an offline copy that the people who read the backups cannot reach, and keep every key a
   live backup still needs (see the [secrets guide](../operations/secrets.md#escrow)).
7. **Data residency and storage places.** Decide where the database, the ledger directory
   (`privacy.ledger_dir`) and the backups live, put the ledger directory on durable storage shared by
   every worker, and back it up apart from the database.
8. **Breach handling.** Write an incident plan that says who decides, who is told within your legal
   deadline, and how you use the audit log and the trace to find what was reached.
9. **Requests received outside the product.** Name who receives access and erasure requests that
   arrive by mail or phone, check the requester's identity, and enter the request in the product
   within your deadline.
10. **Access review of members and keys.** Review members, `view_pii` grants and API keys on a schedule,
    and remove or narrow whatever is not needed.
11. **Retention windows the business needs.** Set each window to the shortest period you can justify,
    within the bounds in E14, and record the reason.
12. **Erasure in systems Zirv Flow does not call (added).** Erase the person at webhook and SMTP
    destinations, in exports and downloads you made, and in any copy you hold elsewhere, because the
    product removes only what it holds and calls only destinations that declare erase support.
