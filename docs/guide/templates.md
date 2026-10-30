# Templates

Use this guide to build an email, check how it looks for different people, send a
test and review the change before it goes live. An editor can draft and test; a
publisher can publish. Real-person previews and tests also require `view_pii`
(or the `pii` API-key scope), implied for owners and workspace admins.

## Worked example: a newsletter

1. Have an admin prepare a verified sender identity and a sending destination
   using the [channels guide](channels.md).
2. Open **Templates**, create an email in blocks mode and name it “Monthly news”.
   Add a heading, rich text and a button linking to your own HTTPS site. In
   **Message**, set the subject, preheader, language and sender.
3. Choose the workspace default theme, or a published named theme. Keep its
   unsubscribe footer. Preview desktop and mobile widths with a sample person;
   check any personalization with a permitted real-person preview too.
4. Choose **Send a test**, select a team member or allowlisted test inbox, the
   destination and sender, then send. Check the result and the received email.
5. Open **Review and publish**. Resolve blocking checks, review warnings and
   **Where it goes live**, add a version note if useful, and publish.
   A published flow picks up this version on subsequent sends.

## Themes, snippets, assets and conditions

Open **Content library** under **Messaging** for Themes, Snippets and Images.
The library tabs also link to Templates.

A block document has sections with one to three columns. Add blocks from the
palette, reorder them by drag or keyboard, and edit their settings. The draft
saves automatically; wait for **Draft saved** before leaving. If another edit
wins, the editor keeps your unsaved work and offers **Load the newer draft**.

**Themes** supply brand colours, typography, a logo and footer. A template can
follow the workspace default or a named theme; a document without a theme uses
its own style settings. Publishing a theme affects future renders of templates
that use it. Review where it is used before changing shared branding.

**Snippets** are reusable published block content. Insert one through the snippet
picker; its content is resolved during rendering. Publishing a snippet changes
future renders wherever it is referenced. Where-used and version history help
review the reach. Messages record the theme and snippet versions they rendered.

**Assets** hold uploaded PNG, JPEG, GIF and WebP images. SVG is refused. Choose an
asset or the theme logo for an image and supply meaningful alt text. The server
serves assets under `/a/`, so a deployed `server.public_url` must be reachable by
recipients. Uploads default to 5 MiB per image and 1 GiB per workspace, controlled
by `assets.max_file_bytes` and `assets.max_workspace_bytes`; oversized uploads
fail with `asset_too_large` or `asset_quota_exceeded`.

**Conditions** use **Show: Always** or **Only when** on a block or section. They
use the bounded segment condition language, with its missing-value semantics;
preview the people who should see and should not see the content. Hidden content
is left out of the rendered message, and preview reports why it was hidden.
A condition is not a substitute for consent or a send limit.

## Review and versions

**Review and publish** compares the draft with the live version, checks a sample
person and the saved preview people, and lists active flows with recent send
volume. That volume is a historical average, not a forecast. Blocking issues stop
publication; warnings remain reviewable. A failed check must be retried, not
interpreted as a pass. Published versions are immutable; **Versions and use**
shows history and where the template is used. An active flow prevents archiving
its template until the reference is removed.

The renderer is shared by preview, tests and sends. It runs in an isolated
[render host](../operations/runtime.md#render-host), and the browser shows the
result in a sandboxed frame. It checks merge fields, URLs, document shape and
required unsubscribe content. **Convert to MJML** is a one-way draft conversion;
review the result before publishing it.

## Template editor and delivery counts

The web app has a Templates page. A template is written as HTML, MJML or from blocks (chosen when it is made) in a split view:
the editor on the left, the preview on the right. The block editor has a palette, a canvas you can
reorder by drag or by keyboard, a settings panel and inline validation, and a one-way "Convert to
MJML" action turns a block template into MJML code. The draft is saved as you type and the server renders it again for the preview, shown in a
sandboxed iframe, so the app never renders template HTML itself. Editors can preview and test, publishers can publish, and real-person rendering also needs personal-data access. A flow's send step chooses a published template and a destination; a sync step
chooses a destination and the traits to keep in an external system.

`GET /api/v1/workspaces/{ws}/flows/{id}/versions/{n}/counts` also returns, per node, the delivery counts
read from the version's message rows when asked: `sent`, `send_failed` and `skipped`, a list of
`{reason, count}` sorted by reason. Test messages and messages still pending or sending are not counted.

## Template test sends

A test send sends one template through a real destination to one person who agreed to receive it: a
member of the workspace, at the address on their account, or an address an admin put on the allowlist.
The subject starts with `[TEST] `, links are not rewritten, there is no open pixel, and nothing is
written to the message log, the events or the caps. `{{ system.unsubscribe_url }}` and the
`List-Unsubscribe` header point to a `/u/{token}` link that answers "This was a test send. Nothing was
changed." and does nothing.

| Route | Needs | |
|-------|-------|-|
| `POST /templates/{id}/test-send` | write (personal-data access with `person_id`) | body: `version` (absent: the draft), `destination_id`, `sender_identity_id`, `to` (`{"member": id}`, `{"allowlist_entry": id}` or `{"address": "..."}`), `person_id` (optional; absent renders the sample person); answers 202 with the log row |
| `GET /test-sends` | read | the log, newest first; `template_id`, `limit`, `cursor` |
| `GET`, `POST /test-send-allowlist`, `DELETE /test-send-allowlist/{id}` | admin | `{"address"}` to add; the list shows only a masked hint (`q***@example.com`) |
| `GET /test-send-allowlist/options` | write | the same entries as `{id, name}`, `name` being the masked hint, for the Send a test page |

The **Send a test** page submits each selected recipient separately. It stops the
remaining sends on a 429. `GET /api/v1/workspaces/{ws}/test-sends/quota` shows
member/workspace usage and the reset time. Consent, quiet hours and frequency caps
are skipped for tests; suppressions still apply. Routes in the table are relative
to the workspace API path.

Any other recipient is a 422 `recipient_not_allowed`. At most 10 test sends per member and 100 per
workspace start in any rolling hour; past that the answer is 429 with `Retry-After`. A suppressed
recipient is never sent to (`suppressed`), and an unverified sender ends `sender_unverified`. The job
checks the recipient again when it runs: a member must still be a member (and the account's current
address is used) and an allowlist entry must still exist, or the row ends `failed` with
`recipient_unavailable` and nothing is sent. The log keeps the configured window (`retention.test_send_months`, default 13 months). The log stores no address; an
allowlist entry holds the hash, the masked hint and the normalised address sealed with the master key
like a credential, and no job payload ever holds an address. Erasing a person deletes the log rows and
the allowlist entries sent to or holding their addresses, and purges their queued test-send jobs. The
leader ends a `queued` row whose job is dead or gone as `failed: abandoned`.

## AI assistant

Zirv Flow can use a language model to help draft its own artifacts. Everything the assistant
does goes through a provider layer with gates in front of it. The assistant is off until the operator lists a provider and an admin turns it on
for a workspace.

**Deployment configuration.** The operator configures `ai.providers` with `id`,
`kind`, `base_url` and `models`. Each model names `name`, `context_window_tokens`,
`input_price_per_mtok` and `output_price_per_mtok`; select an adapter supported by
[the configuration types](../../crates/zirv-flow-api/src/assistant/config.rs).
The operator allowlists the providers, models and base URLs a
workspace can choose from, with each model's context window and prices (in micro-units of your
currency per million tokens, so `3000000` is 3.00). With no providers the assistant is
unavailable and the UI says so.

```toml
[ai]
rate_limit_per_user_per_minute = 20  # default
max_repair_attempts = 2              # default; each repair is a separate, gated call
```

Provider ids and the model names within one provider must be unique, and a base URL is an
http(s) URL without credentials, a query or a fragment. A mistake fails startup and
`zirv-flow config check` with the key and the reason. A compatible local model server can be configured by its `base_url`. **A local server on a private address is
blocked until its address is in `egress.allow`** (see [Egress](sources.md#egress)); the model client never follows
redirects and ignores proxy environment variables.

**Per workspace.** Open **Settings → AI assistant** in the web UI (workspace admins and the owner). An admin chooses
the provider and model, stores the provider key, sets a monthly budget and turns the assistant
on. The key is stored encrypted like any other secret (see [Secrets](../operations/secrets.md)) and is never shown again;
this needs the master key. Turning the assistant on, or changing its provider, model or key while
it is on, runs a short check: the model must call a tool with valid arguments and report token
usage, and its configured context window must be at least 32,768 tokens. A model that fails is
refused with the missing capabilities and their reasons. The settings are
`GET` and `PUT /api/v1/workspaces/{ws}/ai/settings`; `GET .../ai/models` lists the allowlist and
`GET .../ai/usage` the month's spend. All three are admin-only, and a settings change is audited
as `ai.settings.updated` with the changed fields and their new values, never the key.

**Privacy.** By default the server supplies schema context rather than person values.
Authored template copy, prompts and pasted documentation still reach the provider.
Server-supplied personal values require *Allow personal data in requests*; protected
condition literals use placeholders while that setting is off. Pasted documents and samples are fenced and labelled as data, never instructions.
No transcript of a prompt or an answer is stored by default. An admin can opt in to keeping them
for 1 to 30 days (7 is suggested); an hourly job, `maintenance.ai_transcripts`, deletes expired
ones, and erasing a person also deletes the transcripts that name them.

**Budget and rate limit.** Before any request the dispatch checks the assistant is enabled, the
user is under `ai.rate_limit_per_user_per_minute` and the workspace is under its monthly budget
(a calendar month in UTC; a budget of 0 allows no calls). A refused call sends nothing, records
nothing and answers `402` (budget), `429` with `Retry-After` (rate limit), `409` (not enabled) or
`422` (personal data without opt-in). Both counts are read from the call log in the database, so
they hold across instances. Calls in flight reserve the cost of the request's input-token upper
bound and full output cap. A provider that reports usage above either bound can overrun the
budget by the calls in flight; the reported usage is still recorded and counted as a provider
fault. The cost of a call is estimated from the reported tokens and the configured prices.

**The call log.** Every dispatched call writes a row with the workspace, user, feature,
provider, model, token counts, estimated cost, latency and outcome, and never a prompt, an
answer or an error body. The metrics are `zirv_flow_ai_calls_total{feature,outcome}`,
`zirv_flow_ai_refusals_total{reason}` and `zirv_flow_ai_provider_faults_total{feature,reason}`
(`usage_over_reservation` when reported usage exceeds the request's bounds). CI tests use a fake provider and local mock servers. The explicit
assistant evaluation command makes budgeted calls to the configured model.


## Drafting with the assistant

For admins, the template editor offers assistant drafts of email copy, subject variants,
block layouts and translations. Editor sessions can also request and accept template
drafts through the API; the UI currently exposes the panel to admins. Describe the intended change without pasting
personal data. Review the field changes and the before/after renders at desktop
and mobile widths, choose a subject variant, and accept only the changes you want.
Accepting validates again and writes the ordinary template draft; it never
publishes or sends. A translation replaces the current draft, including its
language; keeping a separate translated template is a separate action.

The server limits tools to the selected task and validates the result with bounded
repair calls. Source samples and pasted text are treated as data. With personal
data disabled, template drafting uses the document and catalog, not a real
person's preview values. Personal string literals in visibility conditions are
replaced with placeholders and restored after drafting; the opt-in sends those
conditions as written. Authored copy and prompts can themselves contain personal
data, so review what you put in them. **Keep transcripts** is a
separate opt-in from **Allow personal data in requests**. Even with transcripts
off, the provider receives the request; choose its privacy arrangements separately.
Mapping and manifest drafting is covered in [sources](sources.md#mapping-and-manifest-assistant),
and segment drafting in [segments](segments.md#segment-assistant).

## Limits

- Block documents use schema 2. Reads migrate older supported documents; newer
  schemas are refused. A document is capped at 50 sections, 500 blocks and
  400,000 bytes of text, with additional per-field bounds.
- **Convert to MJML** refuses visibility conditions, snippets and asset/theme image
  references with `not_convertible`. Conversion uses document styles and omits
  the theme footer; restore required unsubscribe content before publishing.
- A preview shows the server render; it cannot guarantee identical rendering in
  every email reader. Check an actual test message before publishing.
- Test sends use real destinations and only approved recipients. They are not
  engagement events and cannot exercise the normal consent/cap gates.
- A render that exceeds `render.deadline_ms` or its host's resource limit fails.
  The operator controls host sizing; the editor cannot bypass it.
- The assistant can be wrong, exhaust its budget or fail validation. Review the
  draft and render diff. Accepting a draft does not grant permission to publish.
