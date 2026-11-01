# API freeze audit

Release evidence for the `/api/v1` freeze ([API contract policy](api.md), issue #406). One row per operation of `crates/zirv-flow-api/openapi.json` at version 0.186.0: 288 operations. Architecture section 15 and the route table `crates/zirv-flow-api/src/rbac.rs` are the references. This includes the Idempotency-Key change (group G1); every creating operation now has a header, a body key, an existing retry guard or a publishing-action exemption recorded in the policy.

## How the table was built

A throwaway script read `openapi.json`, the route table and the Rust sources of `zirv-flow-api`. The Idempotency-Key column was reconciled with the final policy and is held against the document by a no-database test. The rules, so the table can be reproduced and challenged:

- **Idempotency-Key.** `has`: the operation documents an `Idempotency-Key` header parameter. `has (body field)`: the request body carries an `idempotency_key` field instead. `guarded`: a creating `POST` whose existing unique key, revision, set membership or singleton prevents a duplicate, named in [the policy](api.md#idempotency). `exempt action`: publishing a flow, snippet, template or theme may create another version on a retry, and the policy deliberately excludes these actions from the keyed-create set. Creating candidates are `POST`s that answer `201` or whose operation id starts with `create_`, `add_`, `upload_`, `request_` or `enroll_`. Everything else is `not a creating POST`. No candidate remains undecided.
- **Pagination.** Only `GET` operations named `list_*` with a JSON body are lists. `ok` is the shared shape: query `limit` and `cursor`, response `next_cursor`. `bounded collection` is a bare array the policy names; `exempt shape` is reference data the policy names. An `outlier` is anything else; there is none. Streams and file exports are `n/a`.
- **Problem codes.** From the handler (found by its `operation_id`) and the `ApiError::new` calls in the functions it calls in the same module, up to three levels, plus the `bad_request` and `unprocessable` calls of the handler itself: every client error (400, 409, 413, 415, 422, 429) built without a `.with_code(..)`. `ok`: none. `generic (n)`: n errors carry no specific code and answer the generic code of their status ([the policy](api.md#problem-codes)), which every such response gets. Errors raised in other modules and converted by `?` are not followed, so the cell is a lower bound. The credentials handlers live under a path the audit's tooling does not read, so their cell is `not scanned`; their codes are held by the same source test as the rest.
- **Names and types.** Operation level: the path starts with `/api/v1/` (but for the three documented exceptions `/hooks/{id}`, `/healthz` and `/readyz`, see CONTRIBUTING); `update_*` uses `PATCH`; `DELETE` answers `204`; archive is not a `POST` action; a create answers one success status. The operations that break these rules on purpose are the conventions of [the policy](api.md#conventions-that-look-like-exceptions) (`convention G7`). Schema level, for the request and 2xx response schemas the operation reaches: names are snake_case (`exception G9`: the ingestion bodies and the status-class keys); `*_at` properties are `date-time` strings; a string whose description lists its values (and gives no "such as" examples) is a closed set typed as a string, which the policy keeps (`strings G8: n`). `ok` means nothing to report; `documented` means every finding is a convention, exception or policy choice; `issue` means a finding the policy does not cover. Schema findings are listed once in the last section.
- **Security.** The `security` entry against the rbac row: a public or no-principal row lists the empty requirement, an owner row lists `session` only, an ingest row `api_key` only, any other workspace row both; `401` and, for a row that can refuse, `403` are documented (a redirect answering `303` stands for both). The document carries no scopes (`api_key: []`); the scope each row needs is in [the scope matrix](api.md#scopes), generated from the same table.
- **Preview.** `yes` if the operation is tagged `preview` in the document, which a test holds equal to the policy's unfrozen list.

## Counts

| Check | Result (operations) |
|-------|---------------------|
| Idempotency-Key | 247 not a creating POST, 26 guarded, 4 exempt action, 10 has, 1 has (body field) |
| Pagination | 228 n/a, 36 bounded collection, 21 ok, 3 exempt shape |
| Problem codes | 260 ok, 20 generic, 8 not scanned |
| Names and types | 148 ok, 140 documented |
| Security | 288 ok |
| Preview | 271 no, 17 yes |

Outside the document (listed in the route table, unfrozen by policy, not rows below): `GET /a/{file}`, `GET /metrics`, `GET /t/c/{token}`, `GET /t/o/{token}`, `GET /u/{token}`, `GET /u/{token}/preferences`, `POST /u/{token}`, `POST /u/{token}/preferences`.

The problem codes in use are listed in [the policy](api.md#problem-codes), and a test holds the list and the sources together.

## Operations

| Method | Path | Operation id | Idempotency-Key | Pagination | Problem codes | Names and types | Security | Preview |
|--------|------|--------------|-----------------|------------|---------------|-----------------|----------|---------|
| GET | `/api/v1/admin/jobs` | `get_job_depth` | not a creating POST | n/a | ok | ok | ok | yes |
| GET | `/api/v1/admin/jobs/dead` | `list_dead_jobs` | not a creating POST | ok | ok | ok | ok | yes |
| POST | `/api/v1/admin/jobs/{id}/retry` | `retry_job` | not a creating POST | n/a | ok | ok | ok | yes |
| GET | `/api/v1/audit` | `list_instance_audit` | not a creating POST | ok | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/audit/export` | `export_instance_audit` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/destination-kinds` | `list_destination_kinds` | not a creating POST | bounded collection | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/destinations/{id}/feedback` | `receive_destination_feedback` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/ingest/batch` | `ingest_batch` | not a creating POST | n/a | ok | exception G9: 3; strings G8: 1 | ok | no |
| POST | `/api/v1/ingest/identify` | `ingest_identify` | not a creating POST | n/a | ok | exception G9: 3 | ok | no |
| POST | `/api/v1/ingest/track` | `ingest_track` | not a creating POST | n/a | ok | exception G9: 3 | ok | no |
| GET | `/api/v1/oauth/callback` | `finish_source_oauth` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/session` | `get_session` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/session` | `sign_in` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/session` | `sign_out` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/session/methods` | `get_sign_in_methods` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/session/oidc/callback` | `finish_oidc_sign_in` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/session/oidc/start` | `start_oidc_sign_in` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/setup` | `get_setup_status` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/setup` | `complete_setup` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/source-kinds` | `list_source_kinds` | not a creating POST | bounded collection | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/users` | `list_users` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/users` | `create_user` | guarded | n/a | ok | ok | ok | no |
| GET | `/api/v1/version` | `get_version` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces` | `list_workspaces` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces` | `create_workspace` | guarded | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}` | `get_workspace` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/summary` | `get_workspace_summary` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}` | `update_workspace` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/ai/drafts` | `create_ai_draft` | has | n/a | ok | strings G8: 7 | ok | yes |
| POST | `/api/v1/workspaces/{ws}/ai/drafts/{id}/accept` | `accept_ai_draft` | not a creating POST | n/a | ok | strings G8: 4 | ok | yes |
| GET | `/api/v1/workspaces/{ws}/ai/models` | `list_ai_models` | not a creating POST | exempt shape | ok | strings G8: 1 | ok | yes |
| GET | `/api/v1/workspaces/{ws}/ai/settings` | `get_ai_settings` | not a creating POST | n/a | ok | strings G8: 1 | ok | yes |
| PUT | `/api/v1/workspaces/{ws}/ai/settings` | `put_ai_settings` | not a creating POST | n/a | generic (7) | strings G8: 1 | ok | yes |
| GET | `/api/v1/workspaces/{ws}/ai/usage` | `get_ai_usage` | not a creating POST | n/a | ok | strings G8: 1 | ok | yes |
| GET | `/api/v1/workspaces/{ws}/api-keys` | `list_api_keys` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/api-keys` | `create_api_key` | has | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/api-keys/{id}` | `revoke_api_key` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/assets` | `list_assets` | not a creating POST | ok | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/assets` | `upload_asset` | has | n/a | ok | convention G7: 200 for existing, 201 for new | ok | no |
| GET | `/api/v1/workspaces/{ws}/assets/{id}` | `get_asset` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/assets/{id}` | `delete_asset` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/audit` | `list_audit` | not a creating POST | ok | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/audit/actions` | `list_audit_actions` | not a creating POST | exempt shape | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/audit/export` | `export_audit` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/consent/dimensions` | `list_consent_dimensions` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/consent/dimensions` | `create_consent_dimension` | guarded | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/consent/dimensions/{key}` | `update_consent_dimension` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/consent/dimensions/{key}/values` | `add_consent_dimension_value` | guarded | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/consent/dimensions/{key}/values/{value}` | `update_consent_dimension_value` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/consent/export` | `export_consent` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/consent/import` | `import_consent` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/consent/policy/draft` | `get_consent_policy_draft` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/consent/policy/draft` | `save_consent_policy_draft` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/consent/policy/evaluate` | `evaluate_consent_policy` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/consent/policy/publish` | `publish_consent_policy` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/consent/policy/versions` | `list_consent_policy_versions` | not a creating POST | bounded collection | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/contact-definition` | `get_contact_definition` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/contact-definition` | `put_contact_definition` | not a creating POST | n/a | generic (1) | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/contacts` | `list_contacts` | not a creating POST | ok | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/contacts` | `create_contact` | has | n/a | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/contacts/bulk` | `bulk_upsert_contacts` | not a creating POST | n/a | generic (1) | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/contacts/search` | `search_contacts_get` | not a creating POST | n/a | generic (1) | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/contacts/search` | `search_contacts` | not a creating POST | n/a | generic (2) | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/contacts/{key}` | `get_contact` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/contacts/{key}` | `replace_contact` | has | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/contacts/{key}` | `patch_contact` | has | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/contacts/{key}` | `delete_contact` | has | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/credential-kinds` | `list_credential_kinds` | not a creating POST | bounded collection | not scanned | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/credentials` | `list_credentials` | not a creating POST | bounded collection | not scanned | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/credentials` | `create_credential` | guarded | n/a | not scanned | strings G8: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/credentials/{id}` | `get_credential` | not a creating POST | n/a | not scanned | strings G8: 2 | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/credentials/{id}` | `update_credential` | not a creating POST | n/a | not scanned | strings G8: 2 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/credentials/{id}` | `archive_credential` | not a creating POST | n/a | not scanned | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/credentials/{id}/secret` | `rotate_credential` | not a creating POST | n/a | not scanned | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/credentials/{id}/test` | `test_credential` | not a creating POST | n/a | not scanned | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/debugger/stream` | `stream_event_debugger` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/destinations` | `list_destinations` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/destinations` | `create_destination` | guarded | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/destinations/options` | `list_destination_options` | not a creating POST | bounded collection | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/destinations/{id}` | `get_destination` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/destinations/{id}` | `update_destination` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/destinations/{id}` | `archive_destination` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/destinations/{id}/check` | `check_destination` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/destinations/{id}/domain-status` | `get_destination_domain_status` | not a creating POST | n/a | ok | strings G8: 4 | ok | no |
| GET | `/api/v1/workspaces/{ws}/destinations/{id}/stats` | `get_destination_stats` | not a creating POST | n/a | ok | exception G9: 4 | ok | no |
| GET | `/api/v1/workspaces/{ws}/event-policy` | `get_event_policy` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/event-policy` | `put_event_policy` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/event-schemas` | `list_event_schemas` | not a creating POST | bounded collection | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/event-schemas` | `create_event_schema` | guarded | n/a | ok | strings G8: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/event-schemas/{name}` | `get_event_schema` | not a creating POST | n/a | ok | strings G8: 2 | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/event-schemas/{name}` | `update_event_schema` | not a creating POST | n/a | ok | strings G8: 2 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/event-schemas/{name}/draft` | `save_event_schema_draft` | not a creating POST | n/a | ok | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/event-schemas/{name}/versions` | `publish_event_schema` | guarded | n/a | ok | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/events/explore/breakdown` | `explore_event_breakdown` | not a creating POST | n/a | ok | strings G8: 1 | ok | yes |
| POST | `/api/v1/workspaces/{ws}/events/explore/count` | `explore_event_count` | not a creating POST | n/a | generic (1) | strings G8: 1 | ok | yes |
| POST | `/api/v1/workspaces/{ws}/events/explore/export` | `export_events` | not a creating POST | n/a | ok | strings G8: 1 | ok | yes |
| POST | `/api/v1/workspaces/{ws}/events/explore/list` | `explore_event_list` | not a creating POST | n/a | generic (2) | strings G8: 2 | ok | yes |
| GET | `/api/v1/workspaces/{ws}/flows` | `list_flows` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows` | `create_flow` | guarded | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/flows/{id}` | `get_flow` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/flows/{id}` | `update_flow` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/ai/explain` | `explain_flow_with_ai` | not a creating POST | n/a | ok | strings G8: 1 | ok | yes |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/archive` | `archive_flow` | not a creating POST | n/a | ok | convention G7: archive by POST action; strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/enrollments` | `list_flow_enrollments` | not a creating POST | ok | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/enrollments` | `enroll_person` | has (body field) | n/a | generic (1) | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}` | `get_flow_enrollment` | not a creating POST | n/a | ok | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/retry` | `retry_flow_enrollment` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/enrollments/{enrollment_id}/skip` | `skip_flow_enrollment` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/pause` | `pause_flow` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/publish` | `publish_flow` | exempt action | n/a | ok | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/resume` | `resume_flow` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/start` | `start_flow` | not a creating POST | n/a | generic (1) | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/test` | `test_flow` | not a creating POST | n/a | ok | strings G8: 12 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/flows/{id}/warmup` | `set_flow_warmup` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/warmup/open` | `open_flow_warmup` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/validate` | `validate_flow` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/versions` | `list_flow_versions` | not a creating POST | bounded collection | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/versions/{n}` | `get_flow_version` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/counts` | `get_flow_version_counts` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/flows/{id}/versions/{n}/exit-enrollments` | `exit_flow_version_enrollments` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/forwarding/dead-letters` | `list_forwarding_dead_letters` | not a creating POST | ok | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/forwarding/re-drive` | `redrive_forwarding` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/forwarding/rules` | `list_forwarding_rules` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/forwarding/rules` | `create_forwarding_rule` | guarded | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/forwarding/rules/{id}` | `get_forwarding_rule` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/forwarding/rules/{id}` | `update_forwarding_rule` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/forwarding/rules/{id}` | `delete_forwarding_rule` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/forwarding/status` | `get_forwarding_status` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/identity-conflicts` | `list_identity_conflicts` | not a creating POST | ok | ok | strings G8: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/identity-conflicts/{id}` | `get_identity_conflict` | not a creating POST | n/a | ok | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/identity-conflicts/{id}/resolve` | `resolve_identity_conflict` | not a creating POST | n/a | ok | strings G8: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/imports` | `list_imports` | not a creating POST | ok | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/imports` | `upload_import` | has | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/imports/{id}` | `get_import` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/imports/{id}/cancel` | `cancel_import` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/imports/{id}/dry-run` | `dry_run_import` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/imports/{id}/mapping` | `put_import_mapping` | not a creating POST | n/a | generic (2) | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/imports/{id}/rejects` | `list_import_rejects` | not a creating POST | ok | ok | strings G8: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/imports/{id}/rejects.csv` | `download_import_rejects` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/imports/{id}/run` | `run_import` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/ingest-rejects` | `list_ingest_rejects` | not a creating POST | ok | ok | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/ingest-rejects/replay` | `replay_ingest_rejects` | not a creating POST | n/a | generic (4) | strings G8: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/lists` | `list_lists` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/lists` | `create_list` | guarded | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/lists/{id}` | `get_list` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/lists/{id}` | `update_list` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/lists/{id}` | `delete_list` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/lists/{id}/members` | `list_list_members` | not a creating POST | ok | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/lists/{id}/members` | `add_list_members` | guarded | n/a | ok | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/lists/{id}/members/import` | `import_list_members` | not a creating POST | n/a | generic (2) | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/lists/{id}/members/remove` | `remove_list_members` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/members` | `list_members` | not a creating POST | bounded collection | ok | strings G8: 1 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/members/{user_id}` | `set_member_role` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/members/{user_id}` | `remove_member` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/people/search` | `search_people` | not a creating POST | n/a | generic (1) | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}` | `get_person` | not a creating POST | n/a | ok | strings G8: 3 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/people/{id}` | `delete_person` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/consent` | `get_person_consent` | not a creating POST | n/a | ok | strings G8: 2 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/people/{id}/consent` | `set_person_consent` | not a creating POST | n/a | ok | strings G8: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/consent/history` | `get_person_consent_history` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/explain` | `explain_person_segment` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/lists` | `list_person_lists` | not a creating POST | bounded collection | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/lists/history` | `list_person_list_history` | not a creating POST | ok | ok | strings G8: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/messages` | `list_person_messages` | not a creating POST | ok | ok | strings G8: 6 | ok | no |
| GET | `/api/v1/workspaces/{ws}/people/{id}/timeline` | `get_person_timeline` | not a creating POST | n/a | ok | strings G8: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/privacy/erasures` | `list_erasures` | not a creating POST | ok | ok | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/privacy/erasures` | `request_erasure` | has | n/a | ok | strings G8: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/privacy/exports` | `list_exports` | not a creating POST | ok | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/privacy/exports` | `request_export` | has | n/a | ok | strings G8: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/privacy/exports/{id}/download` | `download_export` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/privacy/settings` | `get_privacy_settings` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/privacy/settings` | `put_privacy_settings` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/retention` | `get_retention` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/retention` | `put_retention` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/segments` | `list_segments` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/segments` | `create_segment` | guarded | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/segments/preview` | `preview_segment` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/segments/{id}` | `get_segment` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/segments/{id}` | `update_segment` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/segments/{id}/ai/explain` | `explain_segment_with_ai` | not a creating POST | n/a | ok | ok | ok | yes |
| POST | `/api/v1/workspaces/{ws}/segments/{id}/archive` | `archive_segment` | not a creating POST | n/a | ok | convention G7: archive by POST action | ok | no |
| GET | `/api/v1/workspaces/{ws}/segments/{id}/history` | `list_segment_history` | not a creating POST | ok | ok | strings G8: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/segments/{id}/members` | `list_segment_members` | not a creating POST | ok | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/segments/{id}/refresh` | `refresh_segment` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/segments/{id}/versions` | `list_segment_versions` | not a creating POST | bounded collection | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/segments/{id}/versions/{n}` | `get_segment_version` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/sender-identities` | `list_sender_identities` | not a creating POST | bounded collection | generic (1) | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/sender-identities` | `create_sender_identity` | guarded | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/sender-identities/options` | `list_sender_identity_options` | not a creating POST | bounded collection | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/sender-identities/{id}` | `get_sender_identity` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/sender-identities/{id}` | `update_sender_identity` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/sender-identities/{id}` | `archive_sender_identity` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/sending-domains` | `list_sending_domains` | not a creating POST | bounded collection | ok | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/sending-domains` | `create_sending_domain` | guarded | n/a | ok | strings G8: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/sending-domains/{id}` | `get_sending_domain` | not a creating POST | n/a | ok | strings G8: 2 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/sending-domains/{id}` | `delete_sending_domain` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/sending-domains/{id}/verify` | `verify_sending_domain` | not a creating POST | n/a | ok | strings G8: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/sending/settings` | `get_send_settings` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/sending/settings` | `put_send_settings` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/snippets` | `list_snippets` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/snippets` | `create_snippet` | guarded | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/snippets/{id}` | `get_snippet` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/snippets/{id}` | `update_snippet` | not a creating POST | n/a | ok | convention G7: update by PUT | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/snippets/{id}` | `archive_snippet` | not a creating POST | n/a | ok | convention G7: DELETE answers 200 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/snippets/{id}/draft` | `save_snippet_draft` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/snippets/{id}/preview` | `preview_snippet` | not a creating POST | n/a | ok | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/snippets/{id}/publish` | `publish_snippet` | exempt action | n/a | ok | strings G8: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/snippets/{id}/versions` | `list_snippet_versions` | not a creating POST | bounded collection | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/snippets/{id}/versions/{n}` | `get_snippet_version` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/snippets/{id}/where-used` | `snippet_where_used` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/source-manifests/validate` | `validate_source_manifest` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/sources` | `list_sources` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources` | `create_source` | guarded | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/sources/{id}` | `get_source` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/sources/{id}` | `update_source` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/sources/{id}` | `delete_source` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/discover` | `discover_source_streams` | not a creating POST | n/a | ok | strings G8: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/manifest/sample` | `sample_source_manifest` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/oauth/start` | `start_source_oauth` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/sources/{id}/runs` | `list_sync_runs` | not a creating POST | ok | ok | strings G8: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/runs` | `start_sync_runs` | not a creating POST | n/a | ok | strings G8: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}` | `get_sync_run` | not a creating POST | n/a | ok | strings G8: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/runs/{run_id}/cancel` | `cancel_sync_run` | not a creating POST | n/a | ok | strings G8: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/sources/{id}/streams` | `list_source_streams` | not a creating POST | bounded collection | ok | strings G8: 3 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/sources/{id}/streams/{name}` | `configure_source_stream` | not a creating POST | n/a | ok | strings G8: 4 | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/streams/{name}/preview` | `preview_stream_mapping` | not a creating POST | n/a | generic (1) | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/sources/{id}/test` | `test_source` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/suppressions` | `list_suppressions` | not a creating POST | ok | ok | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/suppressions` | `add_suppression` | guarded | n/a | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/suppressions/check` | `check_suppression` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/suppressions/export` | `export_suppressions` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/suppressions/import` | `import_suppressions` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| POST | `/api/v1/workspaces/{ws}/suppressions/remove` | `remove_suppression` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates` | `list_templates` | not a creating POST | bounded collection | ok | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/templates` | `create_template` | guarded | n/a | ok | strings G8: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}` | `get_template` | not a creating POST | n/a | ok | strings G8: 3 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/templates/{id}` | `update_template` | not a creating POST | n/a | ok | convention G7: update by PUT; strings G8: 3 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/templates/{id}` | `archive_template` | not a creating POST | n/a | ok | convention G7: DELETE answers 200; strings G8: 3 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/templates/{id}/draft` | `save_template_draft` | not a creating POST | n/a | ok | strings G8: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/draft/convert-to-mjml` | `convert_template_draft_to_mjml` | not a creating POST | n/a | ok | strings G8: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/preview` | `preview_template` | not a creating POST | n/a | ok | strings G8: 3 | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/preview-people` | `list_template_preview_people` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/preview-people` | `add_template_preview_person` | guarded | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/templates/{id}/preview-people/{person_id}` | `remove_template_preview_person` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/publish` | `publish_template` | exempt action | n/a | ok | strings G8: 4 | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/send-volume` | `template_send_volume` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/templates/{id}/test-send` | `test_send_template` | not a creating POST | n/a | generic (3) | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/versions` | `list_template_versions` | not a creating POST | bounded collection | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/versions/{n}` | `get_template_version` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/versions/{n}/stats` | `get_template_version_stats` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/templates/{id}/where-used` | `template_where_used` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/test-send-allowlist` | `list_test_send_allowlist` | not a creating POST | exempt shape | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/test-send-allowlist` | `add_test_send_allowlist_entry` | guarded | n/a | generic (1) | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/test-send-allowlist/options` | `list_test_send_allowlist_options` | not a creating POST | bounded collection | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/test-send-allowlist/{id}` | `delete_test_send_allowlist_entry` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/test-sends` | `list_test_sends` | not a creating POST | ok | generic (2) | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/test-sends/quota` | `test_send_quota` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/themes` | `list_themes` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/themes` | `create_theme` | guarded | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/themes/default` | `set_default_theme` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/themes/{id}` | `get_theme` | not a creating POST | n/a | ok | ok | ok | no |
| PUT | `/api/v1/workspaces/{ws}/themes/{id}` | `update_theme` | not a creating POST | n/a | ok | convention G7: update by PUT | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/themes/{id}` | `archive_theme` | not a creating POST | n/a | ok | convention G7: DELETE answers 200 | ok | no |
| PUT | `/api/v1/workspaces/{ws}/themes/{id}/draft` | `save_theme_draft` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/themes/{id}/preview` | `preview_theme` | not a creating POST | n/a | ok | strings G8: 2 | ok | no |
| POST | `/api/v1/workspaces/{ws}/themes/{id}/publish` | `publish_theme` | exempt action | n/a | ok | strings G8: 2 | ok | no |
| GET | `/api/v1/workspaces/{ws}/themes/{id}/versions` | `list_theme_versions` | not a creating POST | bounded collection | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/themes/{id}/versions/{n}` | `get_theme_version` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/themes/{id}/where-used` | `theme_where_used` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/topics` | `list_topics` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/topics` | `create_topic` | guarded | n/a | ok | strings G8: 1 | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/topics/{id}` | `update_topic` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/traces/{trace_id}` | `get_trace` | not a creating POST | n/a | generic (1) | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/traces/{trace_id}/ai/explain` | `explain_trace_with_ai` | not a creating POST | n/a | generic (1) | ok | ok | yes |
| GET | `/api/v1/workspaces/{ws}/traits` | `list_traits` | not a creating POST | bounded collection | ok | strings G8: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/traits` | `create_trait` | guarded | n/a | ok | strings G8: 5 | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/traits/{key}` | `update_trait` | not a creating POST | n/a | ok | strings G8: 3 | ok | no |
| POST | `/api/v1/workspaces/{ws}/traits/{key}/index` | `index_trait` | not a creating POST | n/a | ok | strings G8: 1 | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/traits/{key}/index` | `unindex_trait` | not a creating POST | n/a | ok | convention G7: DELETE answers 200; strings G8: 1 | ok | no |
| GET | `/api/v1/workspaces/{ws}/webhook-signing-secret` | `get_webhook_signing_secret` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/webhook-signing-secret` | `create_webhook_signing_secret` | guarded | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/webhook-signing-secret/rotate` | `rotate_webhook_signing_secret` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/webhooks` | `list_webhooks` | not a creating POST | bounded collection | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/webhooks` | `create_webhook` | guarded | n/a | ok | ok | ok | no |
| GET | `/api/v1/workspaces/{ws}/webhooks/{id}` | `get_webhook` | not a creating POST | n/a | ok | ok | ok | no |
| PATCH | `/api/v1/workspaces/{ws}/webhooks/{id}` | `update_webhook` | not a creating POST | n/a | ok | ok | ok | no |
| DELETE | `/api/v1/workspaces/{ws}/webhooks/{id}` | `delete_webhook` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/api/v1/workspaces/{ws}/webhooks/{id}/rotate-secret` | `rotate_webhook_secret` | not a creating POST | n/a | ok | ok | ok | no |
| GET | `/healthz` | `get_healthz` | not a creating POST | n/a | ok | ok | ok | no |
| POST | `/hooks/{id}` | `receive_webhook` | not a creating POST | n/a | ok | ok | ok | yes |
| GET | `/readyz` | `get_readyz` | not a creating POST | n/a | ok | ok | ok | no |

## Schema findings

| Finding | Operations reaching it |
|---------|------------------------|
| `AddSuppressionRequest.kind string for a closed set (policy G8)` | 1 |
| `AiCallBody.outcome string for a closed set (policy G8)` | 1 |
| `AiFlowExplainBody.source string for a closed set (policy G8)` | 1 |
| `AssetOwnerBody.kind string for a closed set (policy G8)` | 1 |
| `AuditEntryBody.actor_kind string for a closed set (policy G8)` | 2 |
| `BatchItemRequest.anonymousId not snake_case (exception G9)` | 1 |
| `BatchItemRequest.messageId not snake_case (exception G9)` | 1 |
| `BatchItemRequest.type string for a closed set (policy G8)` | 1 |
| `BatchItemRequest.userId not snake_case (exception G9)` | 1 |
| `ConditionBody.outcome string for a closed set (policy G8)` | 1 |
| `ConflictBody.reason string for a closed set (policy G8)` | 3 |
| `ConflictBody.resolution string for a closed set (policy G8)` | 3 |
| `ConsentImportRowBody.reason string for a closed set (policy G8)` | 1 |
| `ConsentStateBody.status string for a closed set (policy G8)` | 3 |
| `ContactFieldBody.identifier string for a closed set (policy G8)` | 2 |
| `ContactIdentifierQuery.kind string for a closed set (policy G8)` | 1 |
| `CreateContactRequest.key string for a closed set (policy G8)` | 1 |
| `CreateEventSchemaRequest.mode string for a closed set (policy G8)` | 1 |
| `CreateTopicRequest.policy string for a closed set (policy G8)` | 1 |
| `CreateTraitRequest.precedence string for a closed set (policy G8)` | 1 |
| `CreateTraitRequest.type string for a closed set (policy G8)` | 1 |
| `CredentialBody.last_test_outcome string for a closed set (policy G8)` | 5 |
| `CredentialTestBody.code string for a closed set (policy G8)` | 1 |
| `CsvImportBody.status string for a closed set (policy G8)` | 7 |
| `DecisionEntryBody.cause_kind string for a closed set (policy G8)` | 1 |
| `DestinationCheckBody.code string for a closed set (policy G8)` | 1 |
| `DomainRecordBody.kind string for a closed set (policy G8)` | 4 |
| `DomainRecordBody.status string for a closed set (policy G8)` | 4 |
| `DomainStatusBody.status string for a closed set (policy G8)` | 1 |
| `DomainStatusRecordBody.kind string for a closed set (policy G8)` | 1 |
| `DomainStatusRecordBody.status string for a closed set (policy G8)` | 1 |
| `DraftBody.status string for a closed set (policy G8)` | 1 |
| `DryRunBody.reason string for a closed set (policy G8)` | 1 |
| `DryRunBody.status string for a closed set (policy G8)` | 1 |
| `EffectBody.kind string for a closed set (policy G8)` | 1 |
| `EnrollmentActionBody.action string for a closed set (policy G8)` | 2 |
| `EnrollmentItemBody.state string for a closed set (policy G8)` | 2 |
| `ErasureBody.status string for a closed set (policy G8)` | 2 |
| `ErasureCallBody.outcome string for a closed set (policy G8)` | 2 |
| `EventEntryBody.validation_state string for a closed set (policy G8)` | 1 |
| `EventSchemaBody.mode string for a closed set (policy G8)` | 6 |
| `ExplainNodeBody.kind string for a closed set (policy G8)` | 1 |
| `ExploreFilterRequest.op string for a closed set (policy G8)` | 4 |
| `ExploredEventBody.validation_state string for a closed set (policy G8)` | 1 |
| `ExportBody.status string for a closed set (policy G8)` | 2 |
| `FieldBody.field_type string for a closed set (policy G8)` | 3 |
| `FieldSpecBody.kind string for a closed set (policy G8)` | 2 |
| `FinalStateBody.status string for a closed set (policy G8)` | 1 |
| `FlowEdge.port string for a closed set (policy G8)` | 11 |
| `FlowObservedBody.outcome string for a closed set (policy G8)` | 1 |
| `FlowTestBody.source string for a closed set (policy G8)` | 1 |
| `FreshnessBody.state string for a closed set (policy G8)` | 3 |
| `GateResultBody.gate string for a closed set (policy G8)` | 1 |
| `GateResultBody.result string for a closed set (policy G8)` | 1 |
| `HistoryEntryBody.source string for a closed set (policy G8)` | 1 |
| `IdentifierInput.kind string for a closed set (policy G8)` | 1 |
| `IdentifierQuery.kind string for a closed set (policy G8)` | 1 |
| `IdentifierRequest.kind string for a closed set (policy G8)` | 2 |
| `IdentifyRequest.anonymousId not snake_case (exception G9)` | 1 |
| `IdentifyRequest.messageId not snake_case (exception G9)` | 1 |
| `IdentifyRequest.userId not snake_case (exception G9)` | 1 |
| `ImportRejectBody.phase string for a closed set (policy G8)` | 1 |
| `ImportRejectErrorBody.reason string for a closed set (policy G8)` | 1 |
| `IndexBody.index_state string for a closed set (policy G8)` | 2 |
| `IngestRejectBody.reason string for a closed set (policy G8)` | 1 |
| `InvalidRowBody.reason string for a closed set (policy G8)` | 1 |
| `ListMemberBody.source string for a closed set (policy G8)` | 1 |
| `ManifestFieldBody.type string for a closed set (policy G8)` | 1 |
| `MemberBody.role string for a closed set (policy G8)` | 1 |
| `MembershipChangeBody.action string for a closed set (policy G8)` | 1 |
| `MembershipChangeBody.source string for a closed set (policy G8)` | 1 |
| `MessageBody.deciding_rule string for a closed set (policy G8)` | 1 |
| `MessageBody.deferred_why string for a closed set (policy G8)` | 1 |
| `MessageBody.status string for a closed set (policy G8)` | 1 |
| `MessageTemplateBody.category string for a closed set (policy G8)` | 1 |
| `MissingBody.capability string for a closed set (policy G8)` | 2 |
| `NotFoundBody.field string for a closed set (policy G8)` | 2 |
| `PathStepBody.outcome string for a closed set (policy G8)` | 1 |
| `PathStepBody.woke_by string for a closed set (policy G8)` | 1 |
| `PersonIdentifier.kind string for a closed set (policy G8)` | 1 |
| `PersonListBody.source string for a closed set (policy G8)` | 2 |
| `PredictionBody.outcome string for a closed set (policy G8)` | 1 |
| `PredictionBody.reason string for a closed set (policy G8)` | 1 |
| `PreviewRecord.result string for a closed set (policy G8)` | 2 |
| `ProfileTraitBody.trait_type string for a closed set (policy G8)` | 1 |
| `PropertyBody.type string for a closed set (policy G8)` | 5 |
| `ProviderBody.kind string for a closed set (policy G8)` | 1 |
| `ProviderRecordBody.status string for a closed set (policy G8)` | 1 |
| `ReadBody.default_policy string for a closed set (policy G8)` | 2 |
| `ReasonBody.reason string for a closed set (policy G8)` | 2 |
| `RunBody.state string for a closed set (policy G8)` | 4 |
| `RunBody.sync_mode string for a closed set (policy G8)` | 4 |
| `RunBody.trigger string for a closed set (policy G8)` | 4 |
| `ScriptResultBody.kind string for a closed set (policy G8)` | 1 |
| `ScriptResultBody.outcome string for a closed set (policy G8)` | 1 |
| `SegmentHistoryItemBody.cause string for a closed set (policy G8)` | 1 |
| `SegmentHistoryItemBody.kind string for a closed set (policy G8)` | 1 |
| `SetConsentRequest.status string for a closed set (policy G8)` | 1 |
| `SetRoleRequest.role string for a closed set (policy G8)` | 1 |
| `SimulatedPersonBody.final_status string for a closed set (policy G8)` | 1 |
| `SimulatedPersonBody.label string for a closed set (policy G8)` | 1 |
| `SimulatedPersonBody.refused string for a closed set (policy G8)` | 1 |
| `SnapshotConsentBody.status string for a closed set (policy G8)` | 1 |
| `SnippetFlowBody.flow_state string for a closed set (policy G8)` | 2 |
| `SnippetPreviewBody.source string for a closed set (policy G8)` | 1 |
| `StatusClassCountsBody.2xx not snake_case (exception G9)` | 1 |
| `StatusClassCountsBody.3xx not snake_case (exception G9)` | 1 |
| `StatusClassCountsBody.4xx not snake_case (exception G9)` | 1 |
| `StatusClassCountsBody.5xx not snake_case (exception G9)` | 1 |
| `StepBody.outcome string for a closed set (policy G8)` | 1 |
| `StreamBody.sync_mode string for a closed set (policy G8)` | 3 |
| `StreamSettingsRequest.sync_mode string for a closed set (policy G8)` | 1 |
| `SuppressionBody.kind string for a closed set (policy G8)` | 1 |
| `SuppressionBody.reason string for a closed set (policy G8)` | 1 |
| `TargetIdentifier.kind string for a closed set (policy G8)` | 2 |
| `TemplateBody.category string for a closed set (policy G8)` | 9 |
| `TemplateBody.channel string for a closed set (policy G8)` | 9 |
| `TemplateDraftBody.content_type string for a closed set (policy G8)` | 7 |
| `TemplateIssueProblem.severity string for a closed set (policy G8)` | 9 |
| `TemplatePreviewBody.content_type string for a closed set (policy G8)` | 1 |
| `TemplatePreviewBody.source string for a closed set (policy G8)` | 1 |
| `TemplateUseBody.flow_state string for a closed set (policy G8)` | 2 |
| `TestSendBody.outcome string for a closed set (policy G8)` | 2 |
| `ThemeFlowBody.flow_state string for a closed set (policy G8)` | 2 |
| `ThemePreviewBody.source string for a closed set (policy G8)` | 1 |
| `TrackRequest.anonymousId not snake_case (exception G9)` | 1 |
| `TrackRequest.messageId not snake_case (exception G9)` | 1 |
| `TrackRequest.userId not snake_case (exception G9)` | 1 |
| `TraitBody.index_state string for a closed set (policy G8)` | 3 |
| `TraitBody.precedence string for a closed set (policy G8)` | 3 |
| `TraitBody.type string for a closed set (policy G8)` | 3 |
| `UnhealthyInputBody.state string for a closed set (policy G8)` | 1 |
| `UsedByBody.kind string for a closed set (policy G8)` | 5 |
| `WaitBody.resolved string for a closed set (policy G8)` | 1 |
| `WindowBody.key string for a closed set (policy G8)` | 2 |
