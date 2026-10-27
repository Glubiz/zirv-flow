# API freeze audit

Release evidence for the `/api/v1` freeze ([API contract policy](api.md), issue #406). One row per operation of `crates/zirv-flow-api/openapi.json` at version 0.178.0: 288 operations. Architecture section 15 and the route table `crates/zirv-flow-api/src/rbac.rs` are the references. This is the state **before** the fixes of phase B; the table is regenerated when they land.

## How the table was built

A throwaway script read `openapi.json`, the route table and the Rust sources of `zirv-flow-api`; the counts below are its output, not hand counts. The rules, so the table can be reproduced and challenged:

- **Idempotency-Key.** `has`: the operation documents an `Idempotency-Key` header parameter. `has (body field)`: the request body carries an `idempotency_key` field instead of the header. `needed`: a `POST` that answers `201`, or whose operation id starts with `create_`, `add_`, `upload_`, `request_` or `enroll_`, and has neither. Everything else is `not a creating POST`; the borderline calls (bulk upserts, `start_*`, ingestion, publish-as-new-version) are discussed in the fix proposals.
- **Pagination.** Only `GET` operations named `list_*` with a JSON body are lists. `ok` is the shared shape: query `limit` and `cursor`, response `next_cursor`. An `outlier` says how it differs: a bare array with no `limit`, the keyset pair `before`/`next_before` or `after`/`next_after`, or an object with no next cursor. Streams and file exports are `n/a`.
- **Problem codes.** From the handler (found by its `operation_id`) and the functions it calls in the same module, up to three levels: every `ApiError` built for a client error (400, 409, 413, 415, 422 and the other 4xx and 5xx a client acts on, but not 401, 403, 404 or 500) either carries a `code` or does not. `ok`: none without. `free-text-only`: every such error is without a code. `partly free-text`: some are. `not snake_case`: a code outside `[a-z][a-z0-9_]*`. Errors raised in other modules and converted by `?` are not followed, so the cell is a lower bound.
- **Names and types.** Operation level: the path starts with `/api/v1/` (but for the three documented exceptions `/hooks/{id}`, `/healthz` and `/readyz`, see CONTRIBUTING); `update_*` uses `PATCH`; `DELETE` answers `204`; archive is not a `POST` action; a create answers one success status. Schema level, for the request and 2xx response schemas the operation reaches: names are snake_case; `*_at` properties are `date-time` strings; a string whose description lists its values (and gives no "such as" examples) is declared an `enum`. Schema findings are listed once in the last section; the cell shows how many an operation reaches (`schema: n`).
- **Security.** The `security` entry against the rbac row: a public or no-principal row lists no scheme, an owner row lists `session` only, an ingest row `api_key` only, any other workspace row both; `401` and, for a row that can refuse, `403` and (in a workspace) `404` are documented. The document carries no scopes (`api_key: []`), so the scope of each row cannot be compared; that is a finding for the whole document, not a row.
- **Preview.** `yes` if the operation is tagged `preview` in the document or listed as unfrozen by [the policy](api.md#unfrozen-surface). The document tags none today, so the `yes` rows are the policy list alone.

## Counts

| Check | Result (operations) |
|-------|---------------------|
| Idempotency-Key | 247 not a creating POST, 36 needed, 4 has, 1 has (body field) |
| Pagination | 228 n/a, 55 outlier, 5 ok |
| Problem codes | 259 ok, 20 free-text-only, 9 partly free-text |
| Names and types | 147 ok, 141 issue |
| Security | 284 ok, 4 issue |
| Preview | 284 no, 4 yes |

Outside the document (listed in the route table, unfrozen by policy, not rows below): `GET /a/{file}`, `GET /metrics`, `GET /t/c/{token}`, `GET /t/o/{token}`, `GET /u/{token}`, `GET /u/{token}/preferences`, `POST /u/{token}`, `POST /u/{token}/preferences`.

Problem codes in use: 110, all snake_case. There is no single list of them yet; see the proposals.

## Operations

| Method | Path | Operation id | Idempotency-Key | Pagination | Problem codes | Names and types | Security | Preview |
|--------|------|--------------|-----------------|------------|---------------|-----------------|----------|---------|
| GET | `/api/v1/admin/jobs` | `get_job_depth` | not a creating POST | n/a | ok | ok | ok | yes |
| GET | `/api/v1/admin/jobs/dead` | `list_dead_jobs` | not a creating POST | outlier: keyset `before`/`next_before` | ok | ok | ok | yes |
| POST | `/api/v1/admin/jobs/{id}/retry` | `retry_job` | not a creating POST | n/a | free-text-only (1 uncoded) | ok | ok | yes |
| GET | `/api/v1/audit` | `list_instance_audit` | not a creating POST | ok | ok | schema: 1 | ok | no |
| GET | `/api/v1/audit/export` | `export_instance_audit` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/destination-kinds` | `list_destination_kinds` | not a creating POST | outlier: bare array, no limit | ok | schema: 1 | ok | no |
| POST | `/api/v1/destinations/{id}/feedback` | `receive_destination_feedback` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/ingest/batch` | `ingest_batch` | not a creating POST | n/a | ok | schema: 4 | ok | no |
| POST | `/api/v1/ingest/identify` | `ingest_identify` | not a creating POST | n/a | ok | schema: 3 | ok | no |
| POST | `/api/v1/ingest/track` | `ingest_track` | not a creating POST | n/a | ok | schema: 3 | ok | no |
| GET | `/api/v1/oauth/callback` | `finish_source_oauth` | not a creating POST | n/a | ok | ok | rbac ADMIN needs session+api_key, lists ['session']; no 401 documented; no 403 documented | no |
| GET | `/api/v1/session` | `get_session` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/session` | `sign_in` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/session` | `sign_out` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/session/methods` | `get_sign_in_methods` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/session/oidc/callback` | `finish_oidc_sign_in` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/session/oidc/start` | `start_oidc_sign_in` | not a creating POST | n/a | free-text-only (1 uncoded) | ok | ok | no |
| GET | `/api/v1/setup` | `get_setup_status` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/setup` | `complete_setup` | not a creating POST | n/a | free-text-only (1 uncoded) | ok | ok | no |
| GET | `/api/v1/source-kinds` | `list_source_kinds` | not a creating POST | outlier: bare array, no limit | ok | schema: 1 | ok | no |
| GET | `/api/v1/users` | `list_users` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/users` | `create_user` | needed | n/a | ok | ok | ok | no |
| GET | `/api/v1/version` | `get_version` | not a creating POST | n/a | ok | ok | no security entry | no |
| GET | `/api/v1/workspaces` | `list_workspaces` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces` | `create_workspace` | needed | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}` | `get_workspace` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}` | `update_workspace` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/ai/drafts` | `create_ai_draft` | needed | n/a | partly free-text (1 uncoded, 3 coded) | schema: 7 | ok | no |
| POST | `/api/v1/workspaces/{ws}/ai/drafts/{id}/accept` | `accept_ai_draft` | not a creating POST | n/a | ok | schema: 4 | ok | no |
| GET | `/api/v1/workspaces/{ws}/ai/models` | `list_ai_models` | not a creating POST | outlier: object without a next cursor | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/ai/settings` | `get_ai_settings` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/ai/settings` | `put_ai_settings` | not a creating POST | n/a | partly free-text (7 uncoded, 1 coded) | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/ai/usage` | `get_ai_usage` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/api-keys` | `list_api_keys` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/api-keys` | `create_api_key` | needed | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/api-keys/{id}` | `revoke_api_key` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/assets` | `list_assets` | not a creating POST | outlier: keyset `before`/`next_before` | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/assets` | `upload_asset` | needed | n/a | ok | upload answers 200 and 201 | ok | no |
| GET | `/api/v1/workspaces/{ws}/assets/{id}` | `get_asset` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/assets/{id}` | `delete_asset` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/audit` | `list_audit` | not a creating POST | ok | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/audit/actions` | `list_audit_actions` | not a creating POST | outlier: object without a next cursor | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/audit/export` | `export_audit` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/consent/dimensions` | `list_consent_dimensions` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/consent/dimensions` | `create_consent_dimension` | needed | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/consent/dimensions/{key}` | `update_consent_dimension` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/consent/dimensions/{key}/values` | `add_consent_dimension_value` | needed | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/consent/dimensions/{key}/values/{value}` | `update_consent_dimension_value` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/consent/export` | `export_consent` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/consent/import` | `import_consent` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/consent/policy/draft` | `get_consent_policy_draft` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/consent/policy/draft` | `save_consent_policy_draft` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/consent/policy/evaluate` | `evaluate_consent_policy` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/consent/policy/publish` | `publish_consent_policy` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/consent/policy/versions` | `list_consent_policy_versions` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/contact-definition` | `get_contact_definition` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/contact-definition` | `put_contact_definition` | not a creating POST | n/a | free-text-only (1 uncoded) | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/contacts` | `list_contacts` | not a creating POST | outlier: keyset `after`/`next_after` | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/contacts` | `create_contact` | has | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/contacts/bulk` | `bulk_upsert_contacts` | not a creating POST | n/a | free-text-only (1 uncoded) | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/contacts/search` | `search_contacts_get` | not a creating POST | n/a | free-text-only (1 uncoded) | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/contacts/search` | `search_contacts` | not a creating POST | n/a | free-text-only (2 uncoded) | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/contacts/{key}` | `get_contact` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/contacts/{key}` | `replace_contact` | has | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/contacts/{key}` | `patch_contact` | has | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/contacts/{key}` | `delete_contact` | has | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/credential-kinds` | `list_credential_kinds` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/credentials` | `list_credentials` | not a creating POST | outlier: bare array, no limit | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/credentials` | `create_credential` | needed | n/a | ok | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/credentials/{id}` | `get_credential` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/credentials/{id}` | `update_credential` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/credentials/{id}` | `archive_credential` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/credentials/{id}/secret` | `rotate_credential` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/credentials/{id}/test` | `test_credential` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/debugger/stream` | `stream_event_debugger` | not a creating POST | n/a | free-text-only (1 uncoded) | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/destinations` | `list_destinations` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/destinations` | `create_destination` | needed | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/destinations/options` | `list_destination_options` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/destinations/{id}` | `get_destination` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/destinations/{id}` | `update_destination` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/destinations/{id}` | `archive_destination` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/destinations/{id}/check` | `check_destination` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/destinations/{id}/domain-status` | `get_destination_domain_status` | not a creating POST | n/a | partly free-text (1 uncoded, 1 coded) | schema: 4 | ok | no |
| GET | `/api/v1/workspaces/{ws}/destinations/{id}/stats` | `get_destination_stats` | not a creating POST | n/a | ok | schema: 4 | ok | no |
| GET | `/api/v1/workspaces/{ws}/event-policy` | `get_event_policy` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/event-policy` | `put_event_policy` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/event-schemas` | `list_event_schemas` | not a creating POST | outlier: bare array, no limit | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/event-schemas` | `create_event_schema` | needed | n/a | ok | schema: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/event-schemas/{name}` | `get_event_schema` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/event-schemas/{name}` | `update_event_schema` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/event-schemas/{name}/draft` | `save_event_schema_draft` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/event-schemas/{name}/versions` | `publish_event_schema` | needed | n/a | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/events/explore/breakdown` | `explore_event_breakdown` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/events/explore/count` | `explore_event_count` | not a creating POST | n/a | free-text-only (1 uncoded) | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/events/explore/export` | `export_events` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/events/explore/list` | `explore_event_list` | not a creating POST | n/a | free-text-only (2 uncoded) | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/flows` | `list_flows` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows` | `create_flow` | needed | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/flows/{id}` | `get_flow` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/flows/{id}` | `update_flow` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/ai/explain` | `explain_flow_with_ai` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/archive` | `archive_flow` | not a creating POST | n/a | ok | archive by POST action; schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/enrollments` | `list_flow_enrollments` | not a creating POST | outlier: keyset `before`/`next_before` | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/enrollments` | `enroll_person` | has (body field) | n/a | free-text-only (1 uncoded) | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}` | `get_flow_enrollment` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/retry` | `retry_flow_enrollment` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/skip` | `skip_flow_enrollment` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/pause` | `pause_flow` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/publish` | `publish_flow` | needed | n/a | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/resume` | `resume_flow` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/start` | `start_flow` | not a creating POST | n/a | free-text-only (1 uncoded) | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/test` | `test_flow` | not a creating POST | n/a | ok | schema: 12 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/validate` | `validate_flow` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/versions` | `list_flow_versions` | not a creating POST | outlier: bare array, no limit | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/versions/{n}` | `get_flow_version` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/counts` | `get_flow_version_counts` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/exit-enrollments` | `exit_flow_version_enrollments` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/forwarding/dead-letters` | `list_forwarding_dead_letters` | not a creating POST | outlier: keyset `before`/`next_before` | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/forwarding/re-drive` | `redrive_forwarding` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/forwarding/rules` | `list_forwarding_rules` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/forwarding/rules` | `create_forwarding_rule` | needed | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/forwarding/rules/{id}` | `get_forwarding_rule` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/forwarding/rules/{id}` | `update_forwarding_rule` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/forwarding/rules/{id}` | `delete_forwarding_rule` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/forwarding/status` | `get_forwarding_status` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/identity-conflicts` | `list_identity_conflicts` | not a creating POST | outlier: keyset `before`/`next_before` | ok | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/identity-conflicts/{id}` | `get_identity_conflict` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/identity-conflicts/{id}/resolve` | `resolve_identity_conflict` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/imports` | `list_imports` | not a creating POST | outlier: keyset `before`/`next_before` | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/imports` | `upload_import` | needed | n/a | free-text-only (1 uncoded) | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/imports/{id}` | `get_import` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/imports/{id}/cancel` | `cancel_import` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/imports/{id}/dry-run` | `dry_run_import` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/imports/{id}/mapping` | `put_import_mapping` | not a creating POST | n/a | partly free-text (2 uncoded, 1 coded) | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/imports/{id}/rejects` | `list_import_rejects` | not a creating POST | outlier: keyset `after`/`next_after` | ok | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/imports/{id}/rejects.csv` | `download_import_rejects` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/imports/{id}/run` | `run_import` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/ingest-rejects` | `list_ingest_rejects` | not a creating POST | outlier: keyset `before`/`next_before` | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/ingest-rejects/replay` | `replay_ingest_rejects` | not a creating POST | n/a | partly free-text (4 uncoded, 1 coded) | schema: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/lists` | `list_lists` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/lists` | `create_list` | needed | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/lists/{id}` | `get_list` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/lists/{id}` | `update_list` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/lists/{id}` | `delete_list` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/lists/{id}/members` | `list_list_members` | not a creating POST | outlier: keyset `before`/`next_before` | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/lists/{id}/members` | `add_list_members` | needed | n/a | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/lists/{id}/members/import` | `import_list_members` | not a creating POST | n/a | free-text-only (4 uncoded) | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/lists/{id}/members/remove` | `remove_list_members` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/members` | `list_members` | not a creating POST | outlier: bare array, no limit | ok | schema: 1 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/members/{user_id}` | `set_member_role` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/members/{user_id}` | `remove_member` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/people/search` | `search_people` | not a creating POST | n/a | partly free-text (1 uncoded, 1 coded) | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}` | `get_person` | not a creating POST | n/a | ok | schema: 3 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/people/{id}` | `delete_person` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/consent` | `get_person_consent` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/people/{id}/consent` | `set_person_consent` | not a creating POST | n/a | ok | schema: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/consent/history` | `get_person_consent_history` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/explain` | `explain_person_segment` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/lists` | `list_person_lists` | not a creating POST | outlier: bare array, no limit | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/lists/history` | `list_person_list_history` | not a creating POST | outlier: keyset `before`/`next_before` | ok | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/messages` | `list_person_messages` | not a creating POST | outlier: keyset `before`/`next_before` | ok | schema: 7 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/timeline` | `get_person_timeline` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/privacy/erasures` | `list_erasures` | not a creating POST | outlier: keyset `before`/`next_before` | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/privacy/erasures` | `request_erasure` | needed | n/a | ok | schema: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/privacy/exports` | `list_exports` | not a creating POST | outlier: keyset `before`/`next_before` | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/privacy/exports` | `request_export` | needed | n/a | ok | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/privacy/exports/{id}/download` | `download_export` | not a creating POST | n/a | free-text-only (2 uncoded) | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/privacy/settings` | `get_privacy_settings` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/privacy/settings` | `put_privacy_settings` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/retention` | `get_retention` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/retention` | `put_retention` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/segments` | `list_segments` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/segments` | `create_segment` | needed | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/segments/preview` | `preview_segment` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/segments/{id}` | `get_segment` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/segments/{id}` | `update_segment` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/segments/{id}/ai/explain` | `explain_segment_with_ai` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/segments/{id}/archive` | `archive_segment` | not a creating POST | n/a | ok | archive by POST action | ok | no |
| GET | `/api/v1/workspaces/{ws}/segments/{id}/history` | `list_segment_history` | not a creating POST | ok | ok | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/segments/{id}/members` | `list_segment_members` | not a creating POST | ok | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/segments/{id}/refresh` | `refresh_segment` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/segments/{id}/versions` | `list_segment_versions` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/segments/{id}/versions/{n}` | `get_segment_version` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/sender-identities` | `list_sender_identities` | not a creating POST | outlier: bare array, no limit | free-text-only (1 uncoded) | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/sender-identities` | `create_sender_identity` | needed | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/sender-identities/options` | `list_sender_identity_options` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/sender-identities/{id}` | `get_sender_identity` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/sender-identities/{id}` | `update_sender_identity` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/sender-identities/{id}` | `archive_sender_identity` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/sending-domains` | `list_sending_domains` | not a creating POST | outlier: bare array, no limit | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/sending-domains` | `create_sending_domain` | needed | n/a | ok | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/sending-domains/{id}` | `get_sending_domain` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/sending-domains/{id}` | `delete_sending_domain` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/sending-domains/{id}/verify` | `verify_sending_domain` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/sending/settings` | `get_send_settings` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/sending/settings` | `put_send_settings` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/snippets` | `list_snippets` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/snippets` | `create_snippet` | needed | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/snippets/{id}` | `get_snippet` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/snippets/{id}` | `update_snippet` | not a creating POST | n/a | ok | update by PUT, not PATCH | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/snippets/{id}` | `archive_snippet` | not a creating POST | n/a | ok | DELETE answers 200, not 204 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/snippets/{id}/draft` | `save_snippet_draft` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/snippets/{id}/preview` | `preview_snippet` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/snippets/{id}/publish` | `publish_snippet` | needed | n/a | ok | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/snippets/{id}/versions` | `list_snippet_versions` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/snippets/{id}/versions/{n}` | `get_snippet_version` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/snippets/{id}/where-used` | `snippet_where_used` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/source-manifests/validate` | `validate_source_manifest` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/sources` | `list_sources` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources` | `create_source` | needed | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/sources/{id}` | `get_source` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/sources/{id}` | `update_source` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/sources/{id}` | `delete_source` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/discover` | `discover_source_streams` | not a creating POST | n/a | ok | schema: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/manifest/sample` | `sample_source_manifest` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/oauth/start` | `start_source_oauth` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/sources/{id}/runs` | `list_sync_runs` | not a creating POST | ok | ok | schema: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/runs` | `start_sync_runs` | not a creating POST | n/a | ok | schema: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}` | `get_sync_run` | not a creating POST | n/a | ok | schema: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}/cancel` | `cancel_sync_run` | not a creating POST | n/a | free-text-only (1 uncoded) | schema: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/sources/{id}/streams` | `list_source_streams` | not a creating POST | outlier: bare array, no limit | ok | schema: 3 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/sources/{id}/streams/{name}` | `configure_source_stream` | not a creating POST | n/a | ok | schema: 4 | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/streams/{name}/preview` | `preview_stream_mapping` | not a creating POST | n/a | partly free-text (1 uncoded, 2 coded) | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/test` | `test_source` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/suppressions` | `list_suppressions` | not a creating POST | outlier: keyset `before`/`next_before` | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/suppressions` | `add_suppression` | needed | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/suppressions/check` | `check_suppression` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/suppressions/export` | `export_suppressions` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/suppressions/import` | `import_suppressions` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/suppressions/remove` | `remove_suppression` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates` | `list_templates` | not a creating POST | outlier: bare array, no limit | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/templates` | `create_template` | needed | n/a | ok | schema: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}` | `get_template` | not a creating POST | n/a | ok | schema: 3 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/templates/{id}` | `update_template` | not a creating POST | n/a | ok | update by PUT, not PATCH; schema: 3 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/templates/{id}` | `archive_template` | not a creating POST | n/a | ok | DELETE answers 200, not 204; schema: 3 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/templates/{id}/draft` | `save_template_draft` | not a creating POST | n/a | ok | schema: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/draft/convert-to-mjml` | `convert_template_draft_to_mjml` | not a creating POST | n/a | ok | schema: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/preview` | `preview_template` | not a creating POST | n/a | ok | schema: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/preview-people` | `list_template_preview_people` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/preview-people` | `add_template_preview_person` | needed | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/templates/{id}/preview-people/{person_id}` | `remove_template_preview_person` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/publish` | `publish_template` | needed | n/a | ok | schema: 4 | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/send-volume` | `template_send_volume` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/test-send` | `test_send_template` | not a creating POST | n/a | partly free-text (3 uncoded, 2 coded) | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/versions` | `list_template_versions` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/versions/{n}` | `get_template_version` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/versions/{n}/stats` | `get_template_version_stats` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/where-used` | `template_where_used` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/test-send-allowlist` | `list_test_send_allowlist` | not a creating POST | outlier: object without a next cursor | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/test-send-allowlist` | `add_test_send_allowlist_entry` | needed | n/a | free-text-only (3 uncoded) | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/test-send-allowlist/options` | `list_test_send_allowlist_options` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/test-send-allowlist/{id}` | `delete_test_send_allowlist_entry` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/test-sends` | `list_test_sends` | not a creating POST | outlier: keyset `before`/`next_before` | free-text-only (2 uncoded) | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/test-sends/quota` | `test_send_quota` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/themes` | `list_themes` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/themes` | `create_theme` | needed | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/themes/default` | `set_default_theme` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/themes/{id}` | `get_theme` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/themes/{id}` | `update_theme` | not a creating POST | n/a | ok | update by PUT, not PATCH | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/themes/{id}` | `archive_theme` | not a creating POST | n/a | ok | DELETE answers 200, not 204 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/themes/{id}/draft` | `save_theme_draft` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/themes/{id}/preview` | `preview_theme` | not a creating POST | n/a | ok | schema: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/themes/{id}/publish` | `publish_theme` | needed | n/a | ok | schema: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/themes/{id}/versions` | `list_theme_versions` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/themes/{id}/versions/{n}` | `get_theme_version` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/themes/{id}/where-used` | `theme_where_used` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/topics` | `list_topics` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/topics` | `create_topic` | needed | n/a | ok | schema: 1 | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/topics/{id}` | `update_topic` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/traces/{trace_id}` | `get_trace` | not a creating POST | n/a | free-text-only (1 uncoded) | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/traces/{trace_id}/ai/explain` | `explain_trace_with_ai` | not a creating POST | n/a | partly free-text (1 uncoded, 1 coded) | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/traits` | `list_traits` | not a creating POST | outlier: bare array, no limit | ok | schema: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/traits` | `create_trait` | needed | n/a | ok | schema: 5 | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/traits/{key}` | `update_trait` | not a creating POST | n/a | ok | schema: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/traits/{key}/index` | `index_trait` | not a creating POST | n/a | ok | schema: 1 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/traits/{key}/index` | `unindex_trait` | not a creating POST | n/a | ok | DELETE answers 200, not 204; schema: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/webhook-signing-secret` | `get_webhook_signing_secret` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/webhook-signing-secret` | `create_webhook_signing_secret` | needed | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/webhook-signing-secret/rotate` | `rotate_webhook_signing_secret` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/webhooks` | `list_webhooks` | not a creating POST | outlier: bare array, no limit | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/webhooks` | `create_webhook` | needed | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/webhooks/{id}` | `get_webhook` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/webhooks/{id}` | `update_webhook` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/webhooks/{id}` | `delete_webhook` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/webhooks/{id}/rotate-secret` | `rotate_webhook_secret` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/healthz` | `get_healthz` | not a creating POST | n/a | ok | ok | no security entry | no |
| POST | `/hooks/{id}` | `receive_webhook` | not a creating POST | n/a | ok | ok | ok | yes |
| GET | `/readyz` | `get_readyz` | not a creating POST | n/a | ok | ok | no security entry | no |

## Schema findings

| Finding | Operations reaching it |
|---------|------------------------|
| `AddSuppressionRequest.kind open string for a closed set` | 1 |
| `AiCallBody.outcome open string for a closed set` | 1 |
| `AiFlowExplainBody.source open string for a closed set` | 1 |
| `AssetOwnerBody.kind open string for a closed set` | 1 |
| `AuditEntryBody.actor_kind open string for a closed set` | 2 |
| `BatchItemRequest.anonymousId not snake_case` | 1 |
| `BatchItemRequest.messageId not snake_case` | 1 |
| `BatchItemRequest.type open string for a closed set` | 1 |
| `BatchItemRequest.userId not snake_case` | 1 |
| `ConditionBody.outcome open string for a closed set` | 1 |
| `ConflictBody.reason open string for a closed set` | 3 |
| `ConflictBody.resolution open string for a closed set` | 3 |
| `ConsentImportRowBody.reason open string for a closed set` | 1 |
| `ConsentStateBody.status open string for a closed set` | 3 |
| `ContactFieldBody.identifier open string for a closed set` | 2 |
| `ContactIdentifierQuery.kind open string for a closed set` | 1 |
| `CreateContactRequest.key open string for a closed set` | 1 |
| `CreateEventSchemaRequest.mode open string for a closed set` | 1 |
| `CreateTopicRequest.policy open string for a closed set` | 1 |
| `CreateTraitRequest.precedence open string for a closed set` | 1 |
| `CreateTraitRequest.type open string for a closed set` | 1 |
| `CredentialBody.last_test_outcome open string for a closed set` | 5 |
| `CredentialTestBody.code open string for a closed set` | 1 |
| `CsvImportBody.status open string for a closed set` | 7 |
| `DebuggerItemBody.outcome open string for a closed set` | 1 |
| `DecisionEntryBody.cause_kind open string for a closed set` | 1 |
| `DestinationCheckBody.code open string for a closed set` | 1 |
| `DomainRecordBody.kind open string for a closed set` | 4 |
| `DomainRecordBody.status open string for a closed set` | 4 |
| `DomainStatusBody.status open string for a closed set` | 1 |
| `DomainStatusRecordBody.kind open string for a closed set` | 1 |
| `DomainStatusRecordBody.status open string for a closed set` | 1 |
| `DryRunBody.reason open string for a closed set` | 1 |
| `DryRunBody.status open string for a closed set` | 1 |
| `EffectBody.kind open string for a closed set` | 1 |
| `EnrollmentActionBody.action open string for a closed set` | 2 |
| `EnrollmentItemBody.state open string for a closed set` | 2 |
| `ErasureBody.status open string for a closed set` | 2 |
| `ErasureCallBody.outcome open string for a closed set` | 2 |
| `EventEntryBody.validation_state open string for a closed set` | 1 |
| `EventSchemaBody.mode open string for a closed set` | 6 |
| `ExplainNodeBody.kind open string for a closed set` | 1 |
| `ExploreFilterRequest.op open string for a closed set` | 4 |
| `ExploredEventBody.validation_state open string for a closed set` | 1 |
| `ExportBody.status open string for a closed set` | 2 |
| `FieldBody.field_type open string for a closed set` | 3 |
| `FieldSpecBody.kind open string for a closed set` | 2 |
| `FinalStateBody.status open string for a closed set` | 1 |
| `FlowEdge.port open string for a closed set` | 11 |
| `FlowObservedBody.outcome open string for a closed set` | 1 |
| `FlowTestBody.source open string for a closed set` | 1 |
| `FreshnessBody.state open string for a closed set` | 3 |
| `GateResultBody.gate open string for a closed set` | 1 |
| `GateResultBody.result open string for a closed set` | 1 |
| `GateResultBody.run_at timestamp without format date-time` | 1 |
| `HistoryEntryBody.source open string for a closed set` | 1 |
| `IdentifierInput.kind open string for a closed set` | 1 |
| `IdentifierQuery.kind open string for a closed set` | 1 |
| `IdentifierRequest.kind open string for a closed set` | 2 |
| `IdentifyRequest.anonymousId not snake_case` | 1 |
| `IdentifyRequest.messageId not snake_case` | 1 |
| `IdentifyRequest.userId not snake_case` | 1 |
| `ImportRejectBody.phase open string for a closed set` | 1 |
| `ImportRejectErrorBody.reason open string for a closed set` | 1 |
| `IndexBody.index_state open string for a closed set` | 2 |
| `IngestRejectBody.reason open string for a closed set` | 1 |
| `InvalidRowBody.reason open string for a closed set` | 1 |
| `ListMemberBody.source open string for a closed set` | 1 |
| `ManifestFieldBody.type open string for a closed set` | 1 |
| `MemberBody.role open string for a closed set` | 1 |
| `MembershipChangeBody.action open string for a closed set` | 1 |
| `MembershipChangeBody.source open string for a closed set` | 1 |
| `MessageBody.deciding_rule open string for a closed set` | 1 |
| `MessageBody.deferred_why open string for a closed set` | 1 |
| `MessageBody.status open string for a closed set` | 1 |
| `MessageTemplateBody.category open string for a closed set` | 1 |
| `MissingBody.capability open string for a closed set` | 2 |
| `NotFoundBody.field open string for a closed set` | 2 |
| `PathStepBody.outcome open string for a closed set` | 1 |
| `PathStepBody.woke_by open string for a closed set` | 1 |
| `PersonIdentifier.kind open string for a closed set` | 1 |
| `PersonListBody.source open string for a closed set` | 2 |
| `PredictionBody.outcome open string for a closed set` | 1 |
| `PredictionBody.reason open string for a closed set` | 1 |
| `PreviewEvent.occurred_at timestamp without format date-time` | 2 |
| `PreviewRecord.result open string for a closed set` | 2 |
| `ProfileTraitBody.trait_type open string for a closed set` | 1 |
| `PropertyBody.type open string for a closed set` | 5 |
| `ProviderBody.kind open string for a closed set` | 1 |
| `ProviderRecordBody.status open string for a closed set` | 1 |
| `ReadBody.default_policy open string for a closed set` | 2 |
| `ReasonBody.reason open string for a closed set` | 2 |
| `ReasonItem.reason open string for a closed set` | 1 |
| `RunBody.state open string for a closed set` | 4 |
| `RunBody.sync_mode open string for a closed set` | 4 |
| `RunBody.trigger open string for a closed set` | 4 |
| `ScriptResultBody.kind open string for a closed set` | 1 |
| `ScriptResultBody.outcome open string for a closed set` | 1 |
| `SegmentHistoryItemBody.cause open string for a closed set` | 1 |
| `SegmentHistoryItemBody.kind open string for a closed set` | 1 |
| `SetConsentRequest.status open string for a closed set` | 1 |
| `SetRoleRequest.role open string for a closed set` | 1 |
| `SimulatedPersonBody.final_status open string for a closed set` | 1 |
| `SimulatedPersonBody.label open string for a closed set` | 1 |
| `SimulatedPersonBody.refused open string for a closed set` | 1 |
| `SnapshotConsentBody.status open string for a closed set` | 1 |
| `SnippetFlowBody.flow_state open string for a closed set` | 2 |
| `SnippetPreviewBody.source open string for a closed set` | 1 |
| `StatusClassCountsBody.2xx not snake_case` | 1 |
| `StatusClassCountsBody.3xx not snake_case` | 1 |
| `StatusClassCountsBody.4xx not snake_case` | 1 |
| `StatusClassCountsBody.5xx not snake_case` | 1 |
| `StepBody.outcome open string for a closed set` | 1 |
| `StreamBody.sync_mode open string for a closed set` | 3 |
| `StreamSettingsRequest.sync_mode open string for a closed set` | 1 |
| `SuppressionBody.kind open string for a closed set` | 1 |
| `SuppressionBody.reason open string for a closed set` | 1 |
| `TargetIdentifier.kind open string for a closed set` | 2 |
| `TemplateBody.category open string for a closed set` | 9 |
| `TemplateBody.channel open string for a closed set` | 9 |
| `TemplateDraftBody.content_type open string for a closed set` | 7 |
| `TemplateIssueProblem.severity open string for a closed set` | 9 |
| `TemplatePreviewBody.content_type open string for a closed set` | 1 |
| `TemplatePreviewBody.source open string for a closed set` | 1 |
| `TemplateUseBody.flow_state open string for a closed set` | 2 |
| `TestSendBody.outcome open string for a closed set` | 2 |
| `ThemeFlowBody.flow_state open string for a closed set` | 2 |
| `ThemePreviewBody.source open string for a closed set` | 1 |
| `TrackRequest.anonymousId not snake_case` | 1 |
| `TrackRequest.messageId not snake_case` | 1 |
| `TrackRequest.userId not snake_case` | 1 |
| `TraitBody.index_state open string for a closed set` | 3 |
| `TraitBody.precedence open string for a closed set` | 3 |
| `TraitBody.type open string for a closed set` | 3 |
| `UnhealthyInputBody.state open string for a closed set` | 1 |
| `UsedByBody.kind open string for a closed set` | 5 |
| `WaitBody.resolved open string for a closed set` | 1 |
| `WindowBody.key open string for a closed set` | 2 |
