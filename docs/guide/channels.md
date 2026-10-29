# Channels

Use this guide to send email from your own domain and understand why a message
was sent, deferred or skipped. A workspace admin sets up domains, identities,
credentials, destinations and sending policy. Editors prepare and test content;
publishers put templates and flows into use.

## Worked example: send a newsletter from your domain

1. In **Sending domains**, add a domain you control. Publish the displayed DKIM,
   SPF and DMARC records in DNS, merging SPF with the existing record. Request a
   verification and wait until the domain is verified.
2. Add a sender identity on that domain using **From name**, **From address** and,
   if needed, **Reply-to (optional)**.
3. In **Credentials**, create an SMTP credential for your relay. In
   **Destinations**, create an SMTP destination using that credential and test it.
   If the relay is private, ask the operator to allow its address in the
   [egress policy](sources.md#egress).
4. In **Sending**, set appropriate frequency caps and quiet hours. Prepare an
   email in **Templates**, choose the verified sender and send a test to a team
   member or approved test inbox. Review and publish the template.
5. Build a flow for the newsletter audience with a send step using this template,
   destination and the intended consent topic. Test the flow, then review and
   publish it; use the [flows guide](flows.md) for audience and enrollment choices.
6. Read the person's message log and trace to see the delivery result or skip
   reason. A successful test does not prove marketing consent: tests bypass
   consent, caps and quiet hours, while live marketing sends enforce them.

The operator must configure an HTTPS `server.public_url` reachable by recipients
and retain the master key: tracking and unsubscribe URLs rely on both. For content
and publish review, see [templates](templates.md).

## SMTP destinations

The `smtp` destination sends email through a mail server or relay. Its workspace
credential contains `host`, `port`, `tls`, `username` and `password`; the
destination's optional `ca_certificate` trusts a private certificate authority.
Typical ports are 587 with `starttls` and 465 with `tls`. STARTTLS must succeed;
there is no fallback to cleartext. `none` is available only to test builds and
is refused in a production build.

The client resolves and checks the address against egress rules at each connection,
verifies TLS against the configured host, authenticates, and signs the message
with the verified domain's DKIM key. It sends HTML and plain text together, with
unsubscribe headers for marketing mail. Its message id and idempotency header
stay the same on retries, but SMTP cannot guarantee the relay deduplicates a
message accepted just before the connection fails. Temporary replies retry;
a permanent recipient failure is a hard bounce.

An HTTP email API instead uses a [provider destination](#email-provider-destinations).
Both use workspace [credentials](sources.md#workspace-credentials), which can be
rotated without replacing the destination.

## Sending domains

An email template publishes only with a sender identity on a verified sending domain (a publish
with any other sender is a 422 `sender_unverified`). Workspace admins manage both:

- `POST /api/v1/workspaces/{ws}/sending-domains` with `{"domain": "example.com"}` (optional
  `selector`, default `zirv`, and `spf_mechanism`, such as `include:spf.relay.example`) generates
  an RSA-2048 DKIM key, seals the private half (it is never returned) and answers with the three
  DNS TXT records to publish: DKIM at `<selector>._domainkey.<domain>`, SPF at the domain and DMARC
  at `_dmarc.<domain>`. The SPF value is a recommendation: a name holds one `v=spf1` record, so
  merge it with the one you have.
- A background job reads those records. The domain is `verified` only while all three are `ok`:
  DKIM when the published `p=` key matches, SPF when exactly one `v=spf1` record contains the
  configured mechanism, DMARC when any `v=DMARC1` record has a valid `p=`. It falls back to
  `unverified` when a check finds one missing or wrong; a name server that does not answer changes
  nothing and the check is retried. `POST .../sending-domains/{id}/verify` queues a check now;
  otherwise the scheduler re-checks every `sending.recheck_hours` (default 6).
- `POST /api/v1/workspaces/{ws}/sender-identities` binds a from name and an address on the domain.
- The pickers read `GET .../sender-identities/options`, `GET .../destinations/options` and
  `GET .../test-send-allowlist/options`. They need the write role (a `write` key, or an editor or above). Each returns
  only ids, names and state (archived ones are listed and marked): never a setting, a credential or an
  address. A destination option and every destination body carry `capabilities`
  `{send, sync, erase}`, derived from the destination type and, for a provider, its manifest; the flow
  editor lists only destinations that can send in an `action.send` step and only those that can sync
  in an `action.sync` step. A read-only member still sees the chosen id.
- `GET` and `PUT /api/v1/workspaces/{ws}/sending/settings` read (any reader) and replace (admins) the
  workspace's frequency caps and quiet hours. A cap is `{channel, window, limit, mode}`: at most
  `limit` messages to one person in any `window` (an ISO 8601 duration in whole hours or days, up to
  `P31D`), and a message over it is `skip`ped (`frequency_cap`) or, in `defer` mode, held until a slot
  frees up. Quiet hours are `{start, end, days}` in the recipient's time zone (their `timezone` trait,
  else the workspace's): a message inside one waits for its end. Transactional mail is exempt from both.

## Tracking and unsubscribe links

Marketing email is sent with signed links on `server.public_url` (the send fails `render_error`
without it, and the secrets master key must be set). A transactional message is not tracked.

- Links are rewritten to `/t/c/{token}` and redirect only to the link recorded for that message;
  `/t/o/{token}` is a 1x1 GIF added at the end of the HTML.
- `{{ system.unsubscribe_url }}` is `/u/{token}`: a GET shows a confirmation and changes nothing, a
  POST unsubscribes. The `List-Unsubscribe` header uses it with `List-Unsubscribe-Post:
  List-Unsubscribe=One-Click` (RFC 8058), so the URL must be `https`.
  `{{ system.preferences_url }}` is `/u/{token}/preferences`, where the person picks topics and
  dimension values; a change to one vertical blocks only that vertical.
- Tokens hold the message id and a link index and a MAC, nothing else. The routes have no sign-in and
  no CSRF token: the token authorises the one message. A bad token is a small 404 page.
- Events: `email.opened` (with `machine` and `machine_reason`), `email.clicked` (with `link_index`) and
  `email.unsubscribed`. An open is a machine open when its user agent contains `proxy`, `prefetch`,
  `preview`, `scanner`, `crawler` or `bot`, or a fragment you list in `tracking.prefetch_user_agents`,
  or when it arrives within 10 seconds of delivery. `machine = false` means not detected as a
  machine, not a verified person. Neither the IP address nor the user agent is stored.

```toml
[tracking]
prefetch_user_agents = ["my-gateway-fetcher"]
```

or `ZIRV_FLOW_TRACKING__PREFETCH_USER_AGENTS='["my-gateway-fetcher"]'`.

### Automatic UTM parameters

A workspace admin can turn on UTM parameters for email links on the Sending screen (the "UTM parameters"
section), or with `PATCH /api/v1/workspaces/{ws}` and a `utm` object. It is off by default. When it is on,
every `http` or `https` link to a listed domain (or a subdomain of one) gets `utm_source`, `utm_medium`,
`utm_campaign` and `utm_content`, in that order, from patterns you set (defaults `{workspace}`, `email`,
`{flow}`, `{template}`). The patterns may only use `{workspace}`, `{flow}`, `{flow_version}`, `{step}`,
`{template}`, `{template_version}` and `{category}`; values are lower-cased with hyphens and URL-encoded, and
nothing about the person is used. Enabling needs at least one domain. Unsubscribe and preference links,
`mailto:`, `tel:` and anchors are never tagged, a `utm_*` parameter already on a link is kept, and
`data-zf-utm="off"` on a link skips it (the attribute is removed from every message, tagged or not, and the same URL in the plain-text part is left alone too). A template can opt out in its editor. The
tagging happens before click tracking, so the recorded link and the redirect carry the parameters, and the
preview and test sends show them too (without a flow, `{flow}` is empty and that parameter is left out).

## Email provider destinations

An email provider with an HTTP API is a destination of type `provider`, described by a manifest
instead of code. The same destination type can also keep people's profiles in an external system
(see Profile sync below). Its `config` is `{"manifest": {...}}`: the manifest format of the HTTP source
(`version`, `base_url`, `auth`, `headers`) with a `destination` section, and no `streams`. Auth is
`none`, `api_key`, `basic` or `bearer`; the manifest's `credential` in `auth` declares the credential
fields, including the secret that signs the provider's feedback, and the destination links a
workspace credential that holds them. The manifest holds no secret. Two fixtures are in
`crates/zirv-flow-connectors/tests/fixtures/providers/`; no real provider's manifest ships yet.

The `destination` section is data only (unknown keys, scripts and `{{ }}` markers are refused, each
issue named by its JSON pointer):

- `send`: `method`, `path`, `idempotency_header` (the message id is sent in it, so a retried
  delivery cannot duplicate at a provider that honours it) and `body`, whose `fields` place `from`,
  `from_name`, `reply_to`, `to`, `subject`, `html`, `text`, `headers` and `message_id` at JSON
  pointers, beside `static` values.
- `rate_limit`: `requests` per `per_seconds`; the default rate of a destination created without one.
- `errors`: rules (`status` or `"500-503"`, an optional `body` pointer and `values`, and an `action`:
  `retry`, `terminal` or `recipient_failure`); the first match wins. By default 408, 429 (which
  honours `Retry-After`) and 5xx retry, any other status is terminal, and a recipient failure is a
  hard bounce.
- `domain_status` (optional): the request that asks the provider about a sending domain, and the
  mapping of its answer to `verified`, `pending` and `failed`.
- `feedback`: how to read the provider's event type, your message id, the bounce kind and an optional
  event id, and `verification` (required): the signature header and prefix, an optional timestamp
  header with `tolerance_seconds`, and the credential field with the signing secret (HMAC-SHA256).

Give the provider `https://<host>/api/v1/destinations/{id}/feedback` as its webhook URL. The request
is public and the signature authorises it: the bounded raw body is verified with the destination's secret before it is
parsed as JSON or recorded, and a missing or wrong signature or a timestamp outside the tolerance is a `401` that
writes nothing. A verified delivery, bounce or complaint is recorded as `email.delivered`,
`email.bounced` (`kind` is `hard` or `soft`) or `email.complained`, once per provider event (a retry
writes nothing). A hard bounce suppresses the address in every workspace of the organization and a
complaint in the destination's workspace; a soft bounce suppresses nobody. The address is the keyed
hash stored on the message when it was sent, so a message sent before this was recorded suppresses
nobody. Events for an unknown message, or one sent through another destination, get a `202` and are
counted in `zirv_flow_provider_feedback_total{outcome}`.

`GET /api/v1/workspaces/{ws}/destinations/{id}/domain-status?domain_id=` (admins only) shows the
provider's status of each DNS record next to ours. It only reads: it never marks a domain verified.

**Profile sync.** The `destination` section may also declare `upsert_profile` and `remove_profile`:
one request each, with a `path` that may hold the placeholder `{key}` once (the external key,
percent-encoded as one path segment), an `idempotency_header`, `key_from` (`{"identity": kind}` or
`{"trait": key}`), a `body` that places the key, the listed traits (`traits` as one object, or
`fields` one by one) and `static` values at JSON pointers, and the same `rate_limit` and `errors` as
`send`. `send` and `feedback` are then optional, as a pair: a manifest declares the email pair, a
profile operation, or both. The `action.sync` flow node (`destination`, `operation` of `upsert` or
`remove`, `trait_keys`, at most 50 and none for a remove) runs the operation through the
`delivery.sync` job, with the rate bucket, retries and verdicts of the other deliveries, and ends in
a `sync.succeeded` or `sync.failed` person event and a decision that hold no key and no trait value.
Keeping a segment in an external system is two flows, entry and exit: [Profile sync](../profile-sync.md) has the
manifest, the worked flows and the details.

**Erase call.** The `destination` section may declare `erase_profile`, with the shape of
`remove_profile` (`key` and `static` body placements only). When a person is erased, every
destination of that manifest that handled them (a message, a forwarded event or a sync) gets one
call, keyed by the `key_from` identity or trait, under an idempotency key that is stable across
retries. A destination without it, including `webhook` and SMTP, gets none.

## Event forwarding

A forwarding rule sends the events of one name to a destination, mapped to the payload you choose.
Only a destination type that carries events accepts a rule (`webhook` does; an email provider does
not: `422 destination_unsupported`). A workspace has at most 20 active rules (`422 too_many_rules`).

A rule has an exact `event_name`, an optional `filter` and a `mapping`:

- `filter` is the segment `where` list (`[{"prop": "total", "op": "gt", "value": 10}]`), all of which
  must hold. It supports `eq`, `neq`, `in`, `not_in`, `contains`, `starts_with`, `exists`,
  `not_exists`, `gt`, `gte`, `lt`, `lte` and `between`, and is checked by the segment validator, so
  a property needs a published event schema. A comparison needs the property to be present and of
  the value's type.
- `mapping` is an object of output key to source: `{"from": "event.id"}`, `event.name`,
  `event.time`, `event.received_at`, `person.id`, `{"from": "event.properties", "pointer":
  "/order/total"}` (a missing property is `null`), or `{"value": "shop"}` (a string, number or
  boolean). There are no expressions and no person traits.

The matching events are copied into an outbox in the transaction that stores them, so a rolled-back
write forwards nothing and a late-committing one is never skipped. This covers client events, synced
events, `webhook.*` and `email.*` events and `segment.entered` and `segment.exited`; a replay adds
none. With no active rule the cost is one indexed lookup per batch. An `event.forward` job per
destination delivers the rows with the webhook's signing, the guarded client and the destination's
rate limit, under the idempotency key `sha256(event id:destination id)`, which every retry and
re-drive reuses (the webhook sends it as `X-Zirv-Idempotency-Key`), so delivery is at least once.
A retryable answer backs off (5 s doubling to an hour, or the `Retry-After`); a terminal answer, or
the eighth attempt, makes the row a dead letter. Each retry and each dead letter appends a decision
(`event.forward`) with ids and a fixed code. Delivered rows lose their payload at once and are
removed after seven days.

All of these are for workspace admins and are audited:

- `GET`, `POST /api/v1/workspaces/{ws}/forwarding/rules` and `GET`, `PATCH`, `DELETE
  /api/v1/workspaces/{ws}/forwarding/rules/{id}`.
- `GET /api/v1/workspaces/{ws}/forwarding/dead-letters?destination_id=&limit=&cursor=` lists dead
  rows newest first with the error code, attempts and times; never the payload.
- `GET /api/v1/workspaces/{ws}/forwarding/status` gives each destination's queue `depth`, the
  `oldest_lag_seconds` of its oldest waiting row and its `dead` count.
- `POST /api/v1/workspaces/{ws}/forwarding/re-drive` with `{"id": ...}` (one row), `{"destination_id":
  ...}` or `{}` (every dead row) puts rows back in the queue with fresh attempts. It writes no
  event and starts no flow.

The metrics `zirv_flow_destination_forwarding_depth` and
`zirv_flow_destination_forwarding_lag_seconds` carry the same numbers, labelled by workspace and
destination id; attempts and outcomes are in `zirv_flow_destination_attempts_total` and
`zirv_flow_destination_outcomes_total`. Erasing a person deletes their outbox rows (the call that
erases them at the destination is separate), and a merge moves them to the survivor.

## Consent and suppressions

The profile store holds what people have agreed to receive and the addresses that
must not receive messages. Live sends check these records before delivery.

**Consent** is kept per person, channel and topic. The only channel is `email`. A state is
`subscribed`, `unsubscribed`, `pending_confirmation` or `unknown` (no row: nobody has said
anything); `subscribed` and `unsubscribed` are the explicit ones. A person has a
**channel-wide** state and a state per **topic**. A channel-wide `unsubscribed` blocks every
topic. Otherwise a topic's own state is read through its policy: an `opt_in` topic needs
`subscribed`, an `opt_out` topic reaches everyone who is not `unsubscribed`.

Every change is appended to a history with its source (`api`, `ui`, `import`, `merge`,
`unsubscribe_link` or `ingest`), the user behind it, when it happened and free-form evidence
(for example `ip`, `user_agent`, `form` or `text`; a JSON object of at most 4096 bytes written compactly). Writing
the status a person already has changes nothing and leaves no history. History rows cannot be
edited. Read and write a person's consent with
`GET` and `PUT /api/v1/workspaces/{ws}/people/{id}/consent` (`{"channel": "email",
"topic_key": "newsletter", "status": "unsubscribed", "evidence": {...}}`; leave out
`topic_key` for the channel as a whole; `unknown` cannot be set) and read the history with
`GET .../consent/history?limit&cursor`. An editor or above, or a `write` key, can record a decision;
the source is `ui` for a session and `api` for a key.

**Merging people** combines their consent for each channel and topic: the most recent
explicit decision wins, and `unsubscribed` wins a tie at the same moment. An explicit
decision beats a state that is not explicit. If the survivor's state changes, the history
gets a `merge` entry naming the merged person, and the merged person's history moves to the
survivor.

**Topics** are what people can subscribe to. Admins manage them under **Topics** in the web
UI or with `GET` and `POST /api/v1/workspaces/{ws}/topics` and `PATCH .../topics/{id}`
(`?archived=true` lists archived ones too). A key is 1 to 64 lowercase letters, digits,
underscores or hyphens and never changes, and neither does the channel; the name, the
description, the policy and the archived flag can change. Every change is audited.

**Consent policy.** A workspace has one consent policy: an ordered list of named, deny-only
rules that decide a marketing send at gate 3, after the recorded consent is read. A rule is a
condition in the segment definition format; the send proceeds when no rule matches, and the first
rule that matches denies it and is named on the message (`deciding_rule`) and in the gate's
decision. The built-in `recorded_consent` rule matches when the recorded consent does not allow
the send; it can be moved but not removed, and a workspace with no published policy has only that
rule. Two leaves are valid in a policy only: `consent` with no channel, whose status is `granted`,
`denied` or `none` for the message being sent, and `messages`, which counts the messages sent to
the person on a channel in a window, or reads the time of the last one, filtered by dimension
values (a cooldown is `{"messages": {"channel": "email", "agg": {"fn": "last_time", "op":
"within_last", "value": "P1D"}}}`). Edit the draft with `GET` and `PUT
/api/v1/workspaces/{ws}/consent/policy/draft` (`write` role), publish it with `POST
.../consent/policy/publish` (`admin`; this appends a `definition_change` decision), list the
versions with `GET .../consent/policy/versions`, and ask what the published policy decides for a
person with `POST .../consent/policy/evaluate` (`admin`; the reply is a rule id and name or none,
and no personal data). A rule that no longer compiles, because something it reads was archived
since it was published, denies the send. A transactional message skips the gate. Admins edit and evaluate the policy in the web UI.

**Suppressions** block an email address or phone number whatever anyone has consented to.
Each reason is one of `hard_bounce`, `complaint`, `manual` or `erased`, and suppression is
per workspace. The value is never stored: a suppression is kept under an HMAC-SHA256 of
`<kind>:<normalized value>`, keyed from the master key (the `identifier` purpose), so it
survives the erasure of the person it came from. This needs the master key; without one the
suppression endpoints answer 503.

- Adding an address that is already suppressed keeps its first reason, except that `erased`
  replaces any other. An `erased` suppression can never be removed (409).
- `POST .../suppressions/check`, `POST .../suppressions` (add) and `POST .../suppressions/remove`
  take `{"kind": "email", "value": "..."}`; they are POSTs so a value never sits in a URL. Checking
  is open to anyone who can read the workspace, changing needs an admin. Notes are at most
  500 characters and must not contain the address.
- `GET .../suppressions?limit&cursor&reason` lists newest first as the first 8 hex characters
  of the hash, never the full hash or the value.
- `POST .../suppressions/import` takes `text/csv` (at most 10 MiB and 100,000 rows) with the
  header `kind,value` or `kind,value,note`. Add `?dry_run=true` to preview. The answer is
  `{added, existing, invalid: [{row, reason}]}`, where `row` is the line of the file the row
  starts on (the header is line 1; blank lines are skipped but counted, and a line break inside
  a quoted value counts) and `reason` a code such as `invalid_email`; values are never echoed.
- `GET .../suppressions/export` (admin) streams `hash,kind,reason,source,created_at`. Because
  values are not stored, the export holds hashes only.

The web UI has a **Topics** page and a **Suppressions** page (check an address, add, import
with a dry-run preview, export, filter by reason). Non-admins see both read-only. The person profile shows consent and its history.

## Limits

- Email and webhook destinations are available; there is no production SMS, push
  or in-app delivery. Provider integrations need a compatible manifest; no
  ready-made manifest for a commercial provider is bundled.
- DNS verification checks configured records, not inbox placement or reputation.
  The scheduler rechecks domains; a later failed verification blocks use of the
  sender until the domain verifies again.
- Marketing sends obey consent, suppression, caps and quiet hours. Transactional
  messages skip consent, caps and quiet hours but still respect suppressions.
  Test sends have their own approved-recipient and rate limits.
- Opens can be automated. `machine = false` means no automation was detected,
  not proof that a person read the email. Transactional mail is not tracked.
- Forwarding is at least once. Receivers should deduplicate the stable idempotency
  key; dead letters need review and re-drive. A provider destination cannot take
  an event-forwarding rule.
- Erasure calls reach only destinations whose manifest declares support. SMTP
  and webhook destinations cannot recall data already delivered.
