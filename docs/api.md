# API contract policy

What a client of `/api/v1` can rely on, and what may still change. The contract is the committed
document [`crates/zirv-flow-api/openapi.json`](../crates/zirv-flow-api/openapi.json) (OpenAPI 3.1);
[architecture section 15](architecture.md#15-api-and-ui) describes the design. The state of every
operation before the freeze is recorded in the [freeze audit](api-audit.md).

Before 1.0 the API may change, but only through the gate below. From 1.0 on, `/api/v1` changes only
additively. A change that is not additive ships as `/api/v2` next to `/api/v1`.

## Frozen after 1.0

For every operation in the document, a released version never changes:

- paths and methods;
- request and response field names and types, and which request fields are required;
- the `type` and `code` values of `application/problem+json` errors;
- auth scopes: `read`, `write`, `publish`, `pii` and `admin` for API keys, and `ingest` for ingestion
  keys, with the role matrix of [architecture 16.2](architecture.md#162-secrets-rbac-audit)
  for sessions, and the scope each route needs ([the scope matrix](#scopes));
- the pagination shape ([Lists and pagination](#lists-and-pagination));
- status codes, and which one answers which condition.

## Additive and allowed

These land in any minor release without notice:

- new paths and methods;
- new optional request fields, and new optional query parameters and headers;
- new response fields;
- new values of a response enum;
- new problem `code` values, and new fields in a problem body;
- new scopes, as long as they grant nothing an existing key lacks.

## Unknown enum values

A client treats an unknown value of a response enum as unknown, never as an error: it shows it
verbatim or falls back to a neutral rendering. The server may add values at any time.

The document does not close a set of values with `enum` where the set may grow. Roles, statuses,
outcomes, kinds and the like are plain `string` properties whose description lists today's values, so
a generated client types them as strings and cannot reject a new one. A property carries an `enum`
only where the set is part of the contract itself (the API key scopes, the segment and flow node
types, the segment operators); the generated client in `web/` renders those as unions, and a new
value there is announced in the release notes. Request enums are the other direction: removing a
value a client may send is breaking; adding one is not.

## Lists and pagination

A list that can grow with the data is paged with one shape: the query parameters `limit` (1 to 200,
default 50) and `cursor`, and a response object with the items under a plural name and
`next_cursor`, null on the last page. A client passes the `next_cursor` of one page as the `cursor` of
the next, treats it as opaque, and keeps the other filters unchanged between pages. Pages run
newest first unless the operation says otherwise.

Two kinds of list are not paged, and are named here so the difference is on record:

- **Bounded collections** are configuration the operator defines, in the dozens to hundreds, and
  every client reads them whole. They answer a bare JSON array and take no `limit` or `cursor`. If
  one ever grows without bound, paging arrives as a new operation or in `/api/v2`, never by changing
  the array:
  `list_destination_kinds`, `list_source_kinds`, `list_users`, `list_workspaces`, `list_api_keys`,
  `list_consent_dimensions`, `list_consent_policy_versions`, `list_credential_kinds`,
  `list_credentials`, `list_destinations`, `list_destination_options`, `list_event_schemas`,
  `list_flows`, `list_flow_versions`, `list_forwarding_rules`, `list_lists`, `list_members`,
  `list_person_lists`, `list_segments`, `list_segment_versions`, `list_sender_identities`,
  `list_sender_identity_options`, `list_sending_domains`, `list_snippets`, `list_snippet_versions`,
  `list_sources`, `list_source_streams`, `list_templates`, `list_template_preview_people`,
  `list_template_versions`, `list_test_send_allowlist_options`, `list_themes`, `list_theme_versions`,
  `list_topics`, `list_traits` and `list_webhooks`.
- **Exempt shapes** are reference data that is an object, not a list of rows: `list_ai_models` (the
  providers and their models), `list_audit_actions` (the audit areas and actions) and
  `list_test_send_allowlist` (the allowlist entries of a workspace).

The paging names of the earlier spellings, `before` and `after` (and `next_before` and `next_after`), no longer exist. A request that still sends `before` or `after` as a query key of a paged list, or `after` in the body of `search_people` or `explore_event_list`, is refused with `400` and the code `paging_parameter_renamed`, not read as the first page, so a client that still pages the old way stops instead of looping. Every other unknown query key stays ignored.

Streams (`stream_event_debugger`) and file exports are not lists.

## Names and types

Field names are snake_case, ids are UUID strings, and a timestamp is an RFC 3339 string in UTC that
the document marks `format: date-time`. Paths are kebab-case nouns under `/api/v1`; a workspace
resource lives under `/api/v1/workspaces/{ws}`.

Three exceptions are part of the contract and are not cleaned up:

- the ingestion bodies of `ingest_identify`, `ingest_track` and `ingest_batch` use the camelCase
  field names of the ingestion SDKs (`anonymousId`, `messageId`, `userId`), for wire compatibility;
- `StatusClassCountsBody` is keyed by HTTP status class (`2xx`, `3xx`, `4xx`, `5xx`);
- `/hooks/{id}`, `/healthz` and `/readyz` sit outside `/api/v1`
  ([CONTRIBUTING](../CONTRIBUTING.md#api-compatibility-gate)).

## Conventions that look like exceptions

These operations differ from their neighbours on purpose. They are frozen as they are.

- A `DELETE` that returns the archived resource answers `200` with it, not `204`:
  `archive_snippet`, `archive_template`, `archive_theme` and `unindex_trait`.
- A whole-document replace is a `PUT`, not a `PATCH`: `update_snippet`, `update_template` and
  `update_theme`.
- Archiving a flow or a segment is a `POST` action with its own guards: `archive_flow` and
  `archive_segment`.
- `upload_asset` answers `201` for new bytes and `200` with the existing row for bytes the
  workspace already holds.

## Unfrozen surface

These may change in any release, 1.x included:

- `/hooks/{id}`: inbound webhooks, shaped by the sender's payload and signature scheme;
- `/t/*`, `/u/*` and `/a/*`: tracking links, unsubscribe pages and image assets, addressed by signed
  tokens and content hashes, not by a contract;
- `/api/v1/admin/jobs` and its sub-paths: operator tooling for the job queue;
- the assistant: every path under `.../ai/` and every `.../ai/explain`;
- the event explorer: `.../events/explore/*`, until its shape has settled;
- the `/__test` routes of the browser-test binary;
- the audit export line format (NDJSON and CSV) until it carries a format version;
- any operation or schema the document tags `preview`.

The document marks the operations of the first five items with the tag `preview`, so a client sees
which operations to hold loosely. A test holds the tag and this list together.

`/healthz` and `/readyz` are probes outside `/api/v1`. They are frozen like any other operation.

## Versioning

`info.version` in the document stays `"1"` for the whole `/api/v1` series. `GET /api/v1/version`
reports the crate version of the running server, which is how a client tells releases apart.

## Deprecation

A deprecated operation, parameter or field stays for at least one minor release after the 1.0
release that deprecates it. While it stays it is marked `deprecated: true` in the document, and the
operation answers with a `Deprecation` header (RFC 9745: `@` and the unix time of the deprecation)
and a `Sunset` header (RFC 8594: the HTTP date of the earliest removal). Removal happens in
`/api/v2`, not in `/api/v1`.

No operation is deprecated today. The helper `zirv_flow_api::deprecation::mark_deprecated` sets both
headers on a response; the first deprecation calls it from the handler, next to the
`deprecated = true` of the operation.

## Idempotency

Creating `POST`s accept an `Idempotency-Key` header (1 to 255 characters). A repeated request with the
same key and body replays the first response for 24 hours; the same key with another body answers
`422` with the code `idempotency_key_reused`.

Which operations count as creating is settled as follows. The creates that nothing else makes safe to
repeat take the key: `create_api_key`, `create_ai_draft`, `upload_asset`, `upload_import`,
`request_erasure`, `request_export`, `create_user`, `create_workspace`, `create_flow`,
`create_segment`, `create_template`, `create_source`, `create_destination`, `create_credential` and
`create_webhook` (and `create_contact`, which had it first). One shared table keyed by workspace,
route and key holds the request hash, the status and the response body; it has no person column.
`create_api_key` stores no body, because its response holds a secret shown once; a replay answers
`409`. A create guarded by a unique name or a draft revision answers `409` on a repeat and takes no
key, and so do the set additions (`add_*`), the publish operations and the ingestion operations
(which de-duplicate on `messageId`).

## Problem codes

Every error is `application/problem+json`. The status is the class of the failure and the `code` is
the stable snake_case name a client branches on. The rule:

- `400`, `409`, `413`, `415`, `422` and `429` always carry a `code`. Where the operation names a
  specific condition the code is specific; where it does not, the code is the generic one of the
  status: `invalid_request` (400), `conflict` (409), `body_too_large` (413), `unsupported_media_type`
  (415), `validation_failed` (422) and `rate_limited` (429).
- `401`, `403`, `404` and `500` are status-only. A client branches on the status, and the body never
  says which part of a credential was wrong or whether a resource exists.
- A `code` is never removed or renamed after 1.0; a condition may move from a generic code to a
  specific one only as a new code that the operation documents.

A test reads the sources and fails when a code used there is missing from this list.

| Code | Meaning |
|------|---------|
| `invalid_request` | A query parameter, path value or body is malformed (generic 400) |
| `paging_parameter_renamed` | A removed paging name (`before`, `after`) was sent; page with `cursor` |
| `conflict` | The state of the resource does not allow the change (generic 409) |
| `body_too_large` | The request body is over the limit (generic 413) |
| `unsupported_media_type` | The content type is not accepted (generic 415) |
| `validation_failed` | A field fails a rule; the detail names the field and the rule (generic 422) |
| `rate_limited` | Too many requests; `Retry-After` says when to retry (generic 429) |
| `idempotency_key_reused` | The `Idempotency-Key` was used with another body |
| `invalid_signature` | An inbound call's signature did not verify |
| `invalid_payload` | An inbound body is not the JSON the receiver needs |
| `dedupe_missing` | The webhook payload has no usable value at the de-duplication field |
| `last_admin` | The change would leave the workspace with no admin |
| `owner_exists` | First-run setup was asked for after an owner exists |
| `name_taken` | A live resource already has that name |
| `secrets_unavailable` | Secrets cannot be stored until the master key is configured |
| `egress_denied` | The egress policy refuses the address a destination or source names |
| `retention_out_of_bounds` | A retention window is outside the deployment's bounds |
| `utm_pattern_invalid` | A UTM parameter pattern is not valid |
| `utm_domain_invalid` | A UTM domain is not a valid domain |
| `utm_domains_required` | UTM parameters are on and no domain is listed |
| `job_already_queued` | An equivalent job is already queued |
| `csv_content_type_required` | A CSV upload did not declare `text/csv` |
| `csv_too_large` | A CSV body is over the limit |
| `not_utf8` | The CSV is not UTF-8 text |
| `record_too_large` | A CSV record is over 1 MiB (an unclosed quote also causes this) |
| `too_many_fields` | A CSV record has more fields than allowed |
| `unterminated_quote` | A quoted CSV field is never closed |
| `blank_header` | A CSV header name is blank |
| `duplicate_header` | Two CSV header names are the same |
| `import_file_too_large` | An import file is over `imports.max_file_bytes` |
| `import_erased` | The import was erased while it was being uploaded |
| `import_job_pending` | A job for the import is still queued or dead |
| `import_state` | The import is not in a state that allows the step |
| `no_identifier` | A mapping has no identifier target |
| `too_many_rules` | A mapping or a workspace has more rules than allowed |
| `unknown_column` | A mapping names a column the file does not have |
| `invalid_identifier_kind` | A mapping names an identifier kind that does not exist |
| `invalid_trait_key` | A mapping names an invalid trait key |
| `unknown_trait` | A mapping names a trait that does not exist |
| `archived_trait` | A mapping names an archived trait |
| `duplicate_trait` | A mapping writes one trait twice |
| `invalid_transform` | A mapping transform is not valid |
| `invalid_event_name` | A mapping event name is not valid |
| `reserved_event_name` | A mapping event name is reserved |
| `invalid_event_property` | A mapping event property is not valid |
| `duplicate_event_property` | A mapping writes one event property twice |
| `too_many_event_properties` | A mapping has more event properties than allowed |
| `trait_slots_full` | All index slots for the trait type are in use |
| `trait_index_retiring` | The trait's index is still being retired |
| `trait_not_indexed` | The trait has no index to remove |
| `trait_indexed` | The trait is already indexed |
| `allowlist_entry_exists` | The address is on the test-send allowlist already |
| `recipient_not_allowed` | A test goes only to a member or an allowlisted address |
| `test_send_rate_limited` | Too many test sends; try again later |
| `sender_identity_exists` | The workspace has a sender identity with that address |
| `sender_identity_archived` | The sender identity is archived |
| `sender_unverified` | The sender identity is not verified yet |
| `sending_domain_exists` | The workspace already has that sending domain |
| `sending_domain_in_use` | A sender identity still uses the sending domain |
| `send_settings_revision_mismatch` | The send settings changed since they were read |
| `export_not_ready` | The export has not finished |
| `source_not_manifest` | The source does not use a manifest |
| `manifest_draft_auth_changed` | The manifest's authentication changed since consent started |
| `oauth_not_configured` | The source has no authorization-code authentication |
| `public_url_not_configured` | `server.public_url` is not set, so a consent cannot start |
| `managed_by_webhooks` | A webhook source is managed through the webhooks API |
| `push_source` | A webhook source receives records as sent and has no run |
| `sync_run_active` | A sync run is active for the source |
| `sync_run_not_active` | The run is not queued or running |
| `stream_not_mapped` | The stream has no mapping |
| `stream_not_enabled` | The stream is not enabled |
| `reconcile_needs_incremental` | Reconciling needs an incremental stream |
| `delete_signal_unsupported` | The stream has no delete signal |
| `invalid_sweep_limit` | The sweep limit is out of range |
| `sample_required` | A webhook stream has no source to read; send a sample payload |
| `sample_not_supported` | Only a webhook stream takes a sample payload |
| `domain_status_unsupported` | The destination declares no domain-status request |
| `destination_archived` | The destination is archived |
| `destination_kind_unavailable` | The destination type is not available in this build |
| `destination_unsupported` | The destination type cannot forward events |
| `credential_archived` | The credential is archived |
| `credential_in_use` | A live source or destination uses the credential |
| `asset_in_use` | A template or theme still uses the asset |
| `asset_too_large` | The image is over the per-file limit |
| `asset_quota_exceeded` | The workspace's images would pass the workspace limit |
| `asset_dimensions_exceeded` | The image's dimensions are over the limit |
| `asset_type_unsupported` | The image type is not accepted |
| `asset_type_mismatch` | The image's bytes do not match its declared type |
| `segment_invalid` | A segment definition fails its checks; `issues` lists them |
| `segment_cycle` | The segment references itself through other segments |
| `segment_referenced` | Other segments or flows reference the segment |
| `segment_archived` | The segment is archived |
| `segment_preview_timeout` | A segment preview ran over its time limit |
| `search_timeout` | A search ran over its time limit |
| `query_timeout` | An explorer query ran over its time limit |
| `pii_trait_not_searchable` | The trait holds personal data and cannot be searched |
| `pii_property_not_explorable` | The event property holds personal data and cannot be explored |
| `debugger_streams_busy` | The workspace has the most debugger streams open on this server |
| `consent_policy_invalid` | A consent policy draft fails its checks; `issues` lists them |
| `consent_policy_revision_mismatch` | The consent policy draft changed since it was read |
| `flow_invalid` | A flow does not pass the publish checks; `flow_issues` lists them |
| `flow_malformed` | A flow document cannot be read |
| `flow_state` | The flow's state does not allow the change |
| `flow_archived` | The flow is archived |
| `flow_paused` | The flow is paused |
| `enrollment_state` | The enrollment's state does not allow the change |
| `flow_test_timeout` | A flow test ran over its time limit |
| `flow_test_values_forbidden` | A flow test may not carry personal values |
| `signing_secret_missing` | The flow has no signing secret to rotate |
| `signing_secret_exists` | The flow already has a signing secret |
| `template_invalid` | A template draft fails the publish checks; `template_issues` lists them |
| `template_malformed` | A template document cannot be read |
| `template_unsupported_schema` | The template document's schema version is not supported |
| `template_unknown_block` | The template uses a block type that does not exist |
| `template_unknown_field` | The template uses a field that does not exist |
| `template_content_unsupported` | The content type does not support the change |
| `template_mismatch` | The template's category and its blocks disagree |
| `template_archived` | The template is archived |
| `template_in_use` | An active flow sends the template |
| `template_draft_changed` | The template draft changed since it was read |
| `template_draft_revision_mismatch` | A stale draft write; `current_revision` says the current one |
| `template_preview_values_forbidden` | A preview may not carry personal values |
| `template_render_failed` | The template did not render |
| `preview_people_limit` | The template has the most preview people allowed |
| `snippet_invalid` | A snippet draft fails its checks |
| `snippet_malformed` | A snippet document cannot be read |
| `snippet_archived` | The snippet is archived |
| `snippet_in_use` | Templates or themes still use the snippet |
| `snippet_draft_changed` | The snippet draft changed since it was read |
| `snippet_revision_mismatch` | A stale snippet write |
| `snippet_breaks_templates` | The new snippet version would stop live templates from sending |
| `theme_invalid` | A theme draft fails its checks |
| `theme_malformed` | A theme document cannot be read |
| `theme_archived` | The theme is archived |
| `theme_is_default` | The default theme cannot be archived |
| `theme_not_publishable_default` | A theme that is the default cannot be published this way |
| `theme_draft_changed` | The theme draft changed since it was read |
| `theme_revision_mismatch` | A stale theme write |
| `theme_breaks_templates` | The new theme version would stop live templates from sending |
| `render_unavailable` | The renderer is not available |
| `render_deadline` | A render ran over its deadline |
| `render_memory_limit` | A render ran over its memory limit |
| `render_request_too_large` | A render request is over the size limit |
| `render_child_exited` | The renderer process exited during the render |
| `render_protocol` | The renderer broke its protocol |
| `ai_disabled` | The assistant is off for the workspace |
| `ai_budget_exceeded` | The assistant's monthly budget is spent |
| `ai_rate_limited` | The assistant is rate limited |
| `ai_user_required` | The assistant needs a signed-in user, not an API key |
| `personal_data_not_allowed` | The request would send personal data to the provider |
| `ai_provider_error` | The provider failed or timed out |
| `ai_capability_missing` | The provider or model lacks a capability the call needs |
| `ai_draft_invalid` | The assistant could not produce a valid draft |
| `ai_draft_outdated` | The target changed since the draft was made |
| `ai_explain_failed` | The assistant could not produce a valid explanation |
| `ai_template_too_large` | The template is too large to send to the assistant |
| `ai_template_unsupported` | The template uses content the assistant cannot handle |
| `ai_flow_too_large` | The flow is too large to send to the assistant |
| `explain_timeout` | An explanation ran over its time limit |

## Scopes

An API key carries independent scopes; they do not imply one another (`admin` alone does not read).
A session is judged by the role of its user in the workspace. The matrix below is generated from
the route table in `crates/zirv-flow-api/src/rbac.rs`, and a test fails when it and the table differ;
regenerate it with `ZIRV_FLOW_BLESS=1 cargo nextest run -p zirv-flow-api scope_matrix`. The document
lists `api_key` and `session` per operation and does not repeat the scope there.

The session column is the lowest role that may call: `member` is any member of the workspace,
`editor`, `publisher` and `admin` are the roles of
[architecture 16.2](architecture.md#162-secrets-rbac-audit), and each role includes the ones before
it. `owner` is the instance owner alone. The key column is the one scope that grants the call; an
`admin` key also reads the member list. Ingestion keys (`ingest`) call the ingestion operations and
nothing else. The `pii` scope does not open a route: it lets a key that also has `read` see masked
values.

<!-- scope-matrix:begin -->

| Method | Path | Session | API key |
|--------|------|---------|---------|
| GET | `/healthz` | none | none |
| GET | `/readyz` | none | none |
| GET | `/metrics` | none | none |
| POST | `/hooks/{id}` | none | none |
| POST | `/api/v1/destinations/{id}/feedback` | none | none |
| GET | `/a/{file}` | none | none |
| GET | `/t/o/{token}` | none | none |
| GET | `/t/c/{token}` | none | none |
| GET | `/u/{token}` | none | none |
| POST | `/u/{token}` | none | none |
| GET | `/u/{token}/preferences` | none | none |
| POST | `/u/{token}/preferences` | none | none |
| GET | `/api/v1/version` | none or any signed-in user | none |
| POST | `/api/v1/ingest/identify` | none | ingest |
| POST | `/api/v1/ingest/track` | none | ingest |
| POST | `/api/v1/ingest/batch` | none | ingest |
| GET | `/api/v1/workspaces/{ws}/webhooks` | member | read |
| POST | `/api/v1/workspaces/{ws}/webhooks` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/webhooks/{id}` | member | read |
| PATCH | `/api/v1/workspaces/{ws}/webhooks/{id}` | admin | admin |
| DELETE | `/api/v1/workspaces/{ws}/webhooks/{id}` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/webhooks/{id}/rotate-secret` | admin | admin |
| GET | `/api/v1/setup` | none or any signed-in user | none |
| POST | `/api/v1/setup` | none or any signed-in user | none |
| GET | `/api/v1/audit` | owner | none |
| GET | `/api/v1/audit/export` | owner | none |
| GET | `/api/v1/workspaces/{ws}/audit` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/audit/actions` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/audit/export` | admin | admin |
| GET | `/api/v1/admin/jobs` | owner | none |
| GET | `/api/v1/admin/jobs/dead` | owner | none |
| POST | `/api/v1/admin/jobs/{id}/retry` | owner | none |
| GET | `/api/v1/session` | none or any signed-in user | none |
| POST | `/api/v1/session` | none or any signed-in user | none |
| DELETE | `/api/v1/session` | none or any signed-in user | none |
| GET | `/api/v1/session/methods` | none or any signed-in user | none |
| GET | `/api/v1/session/oidc/start` | none or any signed-in user | none |
| GET | `/api/v1/session/oidc/callback` | none or any signed-in user | none |
| GET | `/api/v1/users` | owner | none |
| POST | `/api/v1/users` | owner | none |
| GET | `/api/v1/workspaces` | none or any signed-in user | none |
| POST | `/api/v1/workspaces` | owner | none |
| GET | `/api/v1/workspaces/{ws}` | member | read |
| PATCH | `/api/v1/workspaces/{ws}` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/members` | member | read or admin |
| PUT | `/api/v1/workspaces/{ws}/members/{user_id}` | admin | admin |
| DELETE | `/api/v1/workspaces/{ws}/members/{user_id}` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/api-keys` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/api-keys` | admin | admin |
| DELETE | `/api/v1/workspaces/{ws}/api-keys/{id}` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/ai/settings` | admin | admin |
| PUT | `/api/v1/workspaces/{ws}/ai/settings` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/ai/models` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/ai/usage` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/ai/drafts` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/ai/drafts/{id}/accept` | editor | write |
| POST | `/api/v1/workspaces/{ws}/segments/{id}/ai/explain` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/ai/explain` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/traces/{trace_id}/ai/explain` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/traits` | member | read |
| POST | `/api/v1/workspaces/{ws}/traits` | admin | admin |
| PATCH | `/api/v1/workspaces/{ws}/traits/{key}` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/traits/{key}/index` | admin | admin |
| DELETE | `/api/v1/workspaces/{ws}/traits/{key}/index` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/identity-conflicts` | member | read or admin |
| GET | `/api/v1/workspaces/{ws}/identity-conflicts/{id}` | member | read or admin |
| POST | `/api/v1/workspaces/{ws}/identity-conflicts/{id}/resolve` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/people/search` | member | read |
| GET | `/api/v1/workspaces/{ws}/people/{id}` | member | read |
| GET | `/api/v1/workspaces/{ws}/people/{id}/timeline` | member | read |
| GET | `/api/v1/workspaces/{ws}/people/{id}/messages` | member | read |
| GET | `/api/v1/workspaces/{ws}/flows` | member | read |
| POST | `/api/v1/workspaces/{ws}/flows` | editor | write |
| GET | `/api/v1/workspaces/{ws}/flows/{id}` | member | read |
| PATCH | `/api/v1/workspaces/{ws}/flows/{id}` | editor | write |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/validate` | editor | write |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/publish` | publisher | publish |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/pause` | publisher | publish |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/resume` | publisher | publish |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/archive` | publisher | publish |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/enrollments` | member | read |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/enrollments` | editor | write |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}` | member | read |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/retry` | publisher | publish |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/skip` | publisher | publish |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/counts` | member | read |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/exit-enrollments` | publisher | publish |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/start` | publisher | publish |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/test` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/versions` | member | read |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/versions/{n}` | member | read |
| GET | `/api/v1/workspaces/{ws}/webhook-signing-secret` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/webhook-signing-secret` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/webhook-signing-secret/rotate` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/snippets` | member | read |
| POST | `/api/v1/workspaces/{ws}/snippets` | editor | write |
| GET | `/api/v1/workspaces/{ws}/snippets/{id}` | member | read |
| PUT | `/api/v1/workspaces/{ws}/snippets/{id}` | editor | write |
| DELETE | `/api/v1/workspaces/{ws}/snippets/{id}` | admin | admin |
| PUT | `/api/v1/workspaces/{ws}/snippets/{id}/draft` | editor | write |
| POST | `/api/v1/workspaces/{ws}/snippets/{id}/preview` | editor | write |
| POST | `/api/v1/workspaces/{ws}/snippets/{id}/publish` | publisher | publish |
| GET | `/api/v1/workspaces/{ws}/snippets/{id}/versions` | member | read |
| GET | `/api/v1/workspaces/{ws}/snippets/{id}/versions/{n}` | member | read |
| GET | `/api/v1/workspaces/{ws}/snippets/{id}/where-used` | member | read |
| GET | `/api/v1/workspaces/{ws}/themes` | member | read |
| POST | `/api/v1/workspaces/{ws}/themes` | editor | write |
| PUT | `/api/v1/workspaces/{ws}/themes/default` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/themes/{id}` | member | read |
| PUT | `/api/v1/workspaces/{ws}/themes/{id}` | editor | write |
| DELETE | `/api/v1/workspaces/{ws}/themes/{id}` | admin | admin |
| PUT | `/api/v1/workspaces/{ws}/themes/{id}/draft` | editor | write |
| POST | `/api/v1/workspaces/{ws}/themes/{id}/preview` | editor | write |
| POST | `/api/v1/workspaces/{ws}/themes/{id}/publish` | publisher | publish |
| GET | `/api/v1/workspaces/{ws}/themes/{id}/versions` | member | read |
| GET | `/api/v1/workspaces/{ws}/themes/{id}/versions/{n}` | member | read |
| GET | `/api/v1/workspaces/{ws}/themes/{id}/where-used` | member | read |
| GET | `/api/v1/workspaces/{ws}/templates` | member | read |
| POST | `/api/v1/workspaces/{ws}/templates` | editor | write |
| GET | `/api/v1/workspaces/{ws}/templates/{id}` | member | read |
| PUT | `/api/v1/workspaces/{ws}/templates/{id}` | editor | write |
| DELETE | `/api/v1/workspaces/{ws}/templates/{id}` | admin | admin |
| PUT | `/api/v1/workspaces/{ws}/templates/{id}/draft` | editor | write |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/draft/convert-to-mjml` | editor | write |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/publish` | publisher | publish |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/versions` | member | read |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/versions/{n}` | member | read |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/versions/{n}/stats` | member | read |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/where-used` | member | read |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/send-volume` | member | read |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/preview` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/preview-people` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/preview-people` | admin | admin |
| DELETE | `/api/v1/workspaces/{ws}/templates/{id}/preview-people/{person_id}` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/test-send` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/test-sends` | member | read |
| GET | `/api/v1/workspaces/{ws}/test-sends/quota` | member | read |
| GET | `/api/v1/workspaces/{ws}/test-send-allowlist` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/test-send-allowlist` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/test-send-allowlist/options` | editor | write |
| DELETE | `/api/v1/workspaces/{ws}/test-send-allowlist/{id}` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/segments` | member | read |
| POST | `/api/v1/workspaces/{ws}/segments` | editor | write |
| POST | `/api/v1/workspaces/{ws}/segments/preview` | member | read |
| GET | `/api/v1/workspaces/{ws}/segments/{id}` | member | read |
| PATCH | `/api/v1/workspaces/{ws}/segments/{id}` | editor | write |
| POST | `/api/v1/workspaces/{ws}/segments/{id}/archive` | editor | write |
| GET | `/api/v1/workspaces/{ws}/segments/{id}/versions` | member | read |
| GET | `/api/v1/workspaces/{ws}/segments/{id}/versions/{n}` | member | read |
| GET | `/api/v1/workspaces/{ws}/segments/{id}/members` | member | read |
| GET | `/api/v1/workspaces/{ws}/segments/{id}/history` | member | read |
| POST | `/api/v1/workspaces/{ws}/segments/{id}/refresh` | editor | write |
| GET | `/api/v1/workspaces/{ws}/people/{id}/explain` | member | read |
| GET | `/api/v1/workspaces/{ws}/event-schemas` | member | read |
| POST | `/api/v1/workspaces/{ws}/event-schemas` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/event-schemas/{name}` | member | read |
| PATCH | `/api/v1/workspaces/{ws}/event-schemas/{name}` | admin | admin |
| PUT | `/api/v1/workspaces/{ws}/event-schemas/{name}/draft` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/event-schemas/{name}/versions` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/event-policy` | member | read |
| PUT | `/api/v1/workspaces/{ws}/event-policy` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/ingest-rejects` | member | read |
| POST | `/api/v1/workspaces/{ws}/ingest-rejects/replay` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/debugger/stream` | member | read |
| POST | `/api/v1/workspaces/{ws}/events/explore/count` | member | read |
| POST | `/api/v1/workspaces/{ws}/events/explore/breakdown` | member | read |
| POST | `/api/v1/workspaces/{ws}/events/explore/list` | member | read |
| POST | `/api/v1/workspaces/{ws}/events/explore/export` | member | read |
| GET | `/api/v1/workspaces/{ws}/topics` | member | read |
| POST | `/api/v1/workspaces/{ws}/topics` | admin | admin |
| PATCH | `/api/v1/workspaces/{ws}/topics/{id}` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/consent/dimensions` | member | read |
| POST | `/api/v1/workspaces/{ws}/consent/dimensions` | admin | admin |
| PATCH | `/api/v1/workspaces/{ws}/consent/dimensions/{key}` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/consent/dimensions/{key}/values` | admin | admin |
| PATCH | `/api/v1/workspaces/{ws}/consent/dimensions/{key}/values/{value}` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/consent/policy/draft` | member | read |
| PUT | `/api/v1/workspaces/{ws}/consent/policy/draft` | editor | write |
| POST | `/api/v1/workspaces/{ws}/consent/policy/publish` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/consent/policy/versions` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/consent/policy/evaluate` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/consent/import` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/consent/export` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/people/{id}/consent` | member | read |
| PUT | `/api/v1/workspaces/{ws}/people/{id}/consent` | editor | write |
| GET | `/api/v1/workspaces/{ws}/people/{id}/consent/history` | member | read |
| GET | `/api/v1/workspaces/{ws}/suppressions` | member | read |
| POST | `/api/v1/workspaces/{ws}/suppressions` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/suppressions/check` | member | read |
| POST | `/api/v1/workspaces/{ws}/suppressions/remove` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/suppressions/import` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/suppressions/export` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/contact-definition` | member | read |
| PUT | `/api/v1/workspaces/{ws}/contact-definition` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/contacts` | member | read |
| POST | `/api/v1/workspaces/{ws}/contacts` | editor | write |
| POST | `/api/v1/workspaces/{ws}/contacts/bulk` | editor | write |
| GET | `/api/v1/workspaces/{ws}/contacts/search` | member | read |
| POST | `/api/v1/workspaces/{ws}/contacts/search` | member | read |
| GET | `/api/v1/workspaces/{ws}/contacts/{key}` | member | read |
| PUT | `/api/v1/workspaces/{ws}/contacts/{key}` | editor | write |
| PATCH | `/api/v1/workspaces/{ws}/contacts/{key}` | editor | write |
| DELETE | `/api/v1/workspaces/{ws}/contacts/{key}` | editor | write |
| GET | `/api/v1/workspaces/{ws}/destinations/options` | editor | write |
| GET | `/api/v1/workspaces/{ws}/destinations` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/destinations` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/destinations/{id}` | admin | admin |
| PATCH | `/api/v1/workspaces/{ws}/destinations/{id}` | admin | admin |
| DELETE | `/api/v1/workspaces/{ws}/destinations/{id}` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/destinations/{id}/check` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/destinations/{id}/domain-status` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/destinations/{id}/stats` | admin | admin |
| GET | `/api/v1/destination-kinds` | any signed-in user | any scope |
| GET | `/api/v1/workspaces/{ws}/forwarding/rules` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/forwarding/rules` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/forwarding/rules/{id}` | admin | admin |
| PATCH | `/api/v1/workspaces/{ws}/forwarding/rules/{id}` | admin | admin |
| DELETE | `/api/v1/workspaces/{ws}/forwarding/rules/{id}` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/forwarding/dead-letters` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/forwarding/status` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/forwarding/re-drive` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/credential-kinds` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/credentials` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/credentials` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/credentials/{id}` | admin | admin |
| PATCH | `/api/v1/workspaces/{ws}/credentials/{id}` | admin | admin |
| DELETE | `/api/v1/workspaces/{ws}/credentials/{id}` | admin | admin |
| PUT | `/api/v1/workspaces/{ws}/credentials/{id}/secret` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/credentials/{id}/test` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/sending-domains` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/sending-domains` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/sending-domains/{id}` | admin | admin |
| DELETE | `/api/v1/workspaces/{ws}/sending-domains/{id}` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/sending-domains/{id}/verify` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/sender-identities/options` | editor | write |
| GET | `/api/v1/workspaces/{ws}/sender-identities` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/sender-identities` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/sender-identities/{id}` | admin | admin |
| PATCH | `/api/v1/workspaces/{ws}/sender-identities/{id}` | admin | admin |
| DELETE | `/api/v1/workspaces/{ws}/sender-identities/{id}` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/sending/settings` | member | read |
| PUT | `/api/v1/workspaces/{ws}/sending/settings` | admin | admin |
| GET | `/api/v1/source-kinds` | any signed-in user | any scope |
| GET | `/api/v1/workspaces/{ws}/sources` | member | read |
| POST | `/api/v1/workspaces/{ws}/sources` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/sources/{id}` | member | read |
| PATCH | `/api/v1/workspaces/{ws}/sources/{id}` | admin | admin |
| DELETE | `/api/v1/workspaces/{ws}/sources/{id}` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/test` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/discover` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/oauth/start` | admin | admin |
| GET | `/api/v1/oauth/callback` | the user who started the consent | none |
| POST | `/api/v1/workspaces/{ws}/source-manifests/validate` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/manifest/sample` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/sources/{id}/streams` | member | read |
| PUT | `/api/v1/workspaces/{ws}/sources/{id}/streams/{name}` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/streams/{name}/preview` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/sources/{id}/runs` | member | read |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/runs` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}` | member | read |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}/cancel` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/lists` | member | read |
| POST | `/api/v1/workspaces/{ws}/lists` | editor | write |
| GET | `/api/v1/workspaces/{ws}/lists/{id}` | member | read |
| PATCH | `/api/v1/workspaces/{ws}/lists/{id}` | editor | write |
| DELETE | `/api/v1/workspaces/{ws}/lists/{id}` | editor | write |
| GET | `/api/v1/workspaces/{ws}/lists/{id}/members` | member | read |
| POST | `/api/v1/workspaces/{ws}/lists/{id}/members` | editor | write |
| POST | `/api/v1/workspaces/{ws}/lists/{id}/members/remove` | editor | write |
| POST | `/api/v1/workspaces/{ws}/lists/{id}/members/import` | editor | write |
| GET | `/api/v1/workspaces/{ws}/people/{id}/lists` | member | read |
| GET | `/api/v1/workspaces/{ws}/people/{id}/lists/history` | member | read |
| DELETE | `/api/v1/workspaces/{ws}/people/{id}` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/privacy/erasures` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/privacy/erasures` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/privacy/exports` | admin | admin |
| POST | `/api/v1/workspaces/{ws}/privacy/exports` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/privacy/exports/{id}/download` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/privacy/settings` | member | read |
| PUT | `/api/v1/workspaces/{ws}/privacy/settings` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/retention` | admin | admin |
| PUT | `/api/v1/workspaces/{ws}/retention` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/assets` | member | read |
| POST | `/api/v1/workspaces/{ws}/assets` | editor | write |
| GET | `/api/v1/workspaces/{ws}/assets/{id}` | member | read |
| DELETE | `/api/v1/workspaces/{ws}/assets/{id}` | admin | admin |
| GET | `/api/v1/workspaces/{ws}/imports` | member | read |
| POST | `/api/v1/workspaces/{ws}/imports` | editor | write |
| GET | `/api/v1/workspaces/{ws}/imports/{id}` | member | read |
| PUT | `/api/v1/workspaces/{ws}/imports/{id}/mapping` | editor | write |
| POST | `/api/v1/workspaces/{ws}/imports/{id}/dry-run` | editor | write |
| POST | `/api/v1/workspaces/{ws}/imports/{id}/run` | publisher | publish |
| POST | `/api/v1/workspaces/{ws}/imports/{id}/cancel` | editor | write |
| GET | `/api/v1/workspaces/{ws}/imports/{id}/rejects` | member | read |
| GET | `/api/v1/workspaces/{ws}/imports/{id}/rejects.csv` | member | read |
| GET | `/api/v1/workspaces/{ws}/traces/{trace_id}` | member | read |

<!-- scope-matrix:end -->

## The compatibility gate

Every pull request that touches `openapi.json` runs the `OpenAPI Breaking Changes` check
(`openapi-breaking`), which compares the document with the base branch, and once the tags exist with
`v1.0.0-rc.1` and `v1.0.0`, using the pinned `oasdiff` and the severity file
`scripts/openapi-gate/severity.txt`. Removing a path, method or response field, narrowing a type,
requiring a new request field, removing a request enum value, or changing a status or security
contract fails it. New paths, optional fields and new response enum values pass. The full rules and
the local command are in
[CONTRIBUTING](../CONTRIBUTING.md#api-compatibility-gate).

### Exceptions

A deliberate break needs the `api-breaking-approved` label and, in the pull request body, a link to
the decision: a full-SHA commit or blob URL of this repository that changes or points to
`docs/architecture.md`, `docs/roadmap.md` or a Markdown decision under `.zirv/work/`. Until the first
release candidate the maintainer applies the label. After `v1.0.0-rc.1` the operator must approve it,
and the exception is meant for a defect that cannot be fixed additively; each one is recorded in the
decision it links. The goal of the freeze audit is that none is open at rc1.

### Breaking changes before rc1

The freeze audit made these breaking changes, once, before the first release candidate. No external
client existed, and keeping the old spelling beside the new would have frozen both. Each links to the
ruling "#406 phase B rulings" in `.zirv/work/m8-plan/orchestrator-decisions.md`, which is the
decision the pull request body links to.

- Group G3: the paging cursor of 18 `GET` operations is `cursor` and the response field is
  `next_cursor`, where they were `before`/`next_before` (`list_dead_jobs`, `list_assets`,
  `list_flow_enrollments`, `list_forwarding_dead_letters`, `list_identity_conflicts`,
  `list_imports`, `list_ingest_rejects`, `list_list_members`, `get_person_consent_history`,
  `list_person_list_history`, `list_person_messages`, `get_person_timeline`, `list_erasures`,
  `list_exports`, `list_suppressions`, `list_test_sends`) or `after`/`next_after`
  (`list_contacts`, `list_import_rejects`). The request body of `search_people` and
  `explore_event_list` takes `cursor` where it took `after`, and their responses answer
  `next_cursor`, so one spelling is left.
