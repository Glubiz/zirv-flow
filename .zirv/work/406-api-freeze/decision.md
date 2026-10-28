# #406: API contract policy rulings

The binding rulings for the API freeze (#406), recorded by the M8 orchestrator. The G3 paging renames are breaking changes approved by these rulings; later sections narrow G1.

## #406 phase B rulings (2026-10-27; the supervisor could not rule: its reviewer produced no report)

Set R, from `api-freeze-proposals.md`:
- **G1: Idempotency-Key** with replay on the 15 creates that are not naturally idempotent (proposal A). One shared `api_idempotency` table keyed by workspace, route and key, holding the request hash, status and response body. It has no person column; `create_api_key` stores no body, and a replay answers 409. Architecture 15 is amended: creates guarded by a unique name or a draft revision answer 409 and take no key. B (contacts only) is rejected because the issue requires replay tests on two more routes; A costs one migration and one extractor.
- **G2:** bare arrays stay, as documented bounded collections. If one ever grows unbounded, paging arrives as a new endpoint or in v2.
- **G3:** `before`/`after` become `cursor`/`next_cursor` outright before rc1, under the oasdiff override label. No external client exists yet, and an alias would freeze two spellings forever.
- **G5:** 400, 409, 413, 415, 422 and 429 carry a `code`; 401, 403, 404 and 500 are status-only. The 29 operations get codes, and the code list lives in `docs/api.md` with a test.
- **G6, G9:** as proposed. The three ops get `security(())`, the `finish_source_oauth` route row is corrected, and `date-time` formats are added.
- **G7:** the method and status quirks stay, documented as conventions.
- **G8:** no `enum` on closed sets; the unknown-value rule is in the policy.
- **G10:** a `preview` tag on `admin/jobs*`, `/hooks/{id}`, `ai/*` and `*/ai/explain`, and `events/explore/*` until #395's verdict.
- **G11:** scopes are published as a matrix in `docs/api.md`, generated from `rbac.rs` with a drift test, not as per-operation `security` scopes (no oasdiff risk).
- **Timing:** phase B starts after #370 merges, because both touch many api handlers.

## #406 B1: G1 narrowed to six operations (2026-10-28)

The B1 brief asked the worker to drop any create a unique name or draft revision already guards. Nine are guarded, checked against the migrations: `users_email_key` (create_user), workspaces `slug`, and the `(workspace_id, lower(name))` indexes on flows, segments, templates, destinations, credentials and sources (create_source and create_webhook, which creates a source). A retry on those already answers 409, as architecture 15 now states. `Idempotency-Key` ships on the six left: create_api_key, create_ai_draft, upload_asset, upload_import, request_erasure, request_export. Only upload_asset stores a replay body; the other five replay as 409 `idempotency_key_replayed` with the resource id.

## #406 B1 review round 1 (2026-10-28; the supervisor could not rule)

- **Reservation, not a held lock.** A keyed request inserts a pending reservation (its own uuid, a lease) in a short transaction and releases the connection. An overlapping retry answers 409 `idempotency_key_in_flight` at once. The create marks the row done inside its own transaction with `UPDATE ... WHERE reservation = mine`; 0 rows rolls the create back, so a create commits only while it owns the key. A failed request deletes its reservation; an expired lease may be taken over. Rejected: keeping the advisory lock with a `lock_timeout` and a concurrency cap, which stays at-least-once across a crash and still parks connections.
- **No stored bodies.** `upload_asset`'s body holds the client's asset name, which can be a person's value, so it replays as 409 with the resource id like the other five. The table keeps no response column.
- **Principal-scoped.** The key's identity includes the caller (user or API key), so another member cannot replay or block someone else's key.

## #406 B2 review round 1 (2026-10-28)

- **Removed paging names are refused, not ignored.** Both reviewers found that `before`/`after` on the 18 renamed GET lists, and `after` in the `search_people` and `explore_event_list` bodies, fall through to page one. A client that still pages with them would repeat or loop. These names only answer 400 `paging_parameter_renamed`, pointing at `cursor`. Other unknown query keys stay ignored, as today. Rejected: leaving the silent fallback because no external client exists yet; the freeze is exactly when the contract should say no clearly.
- The preview-tag test compares against an explicit list of the ruled 17 operations, not the production predicate.
- The tautological scope-matrix test is dropped; `the_policy_scope_matrix_matches_the_route_table` is the drift test.
