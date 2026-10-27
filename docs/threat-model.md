# Threat model

## Assistant draft-only boundary: review evidence for #391

Catalog names and descriptions, saved artifacts, and template documents are untrusted input.
The prompt renderers separate product headings from fenced data in both privacy modes.
Names are limited to 200 characters after filtering control characters and fence delimiters;
each catalog list has a count limit and an omission count. Filtering never changes the registry.
Schema-only mode excludes source record values and masks personal condition literals.

The server offers exactly one tool and checks the returned tool name and call count before
looking it up. Draft and explain tools validate their arguments and do not write. Accept
revalidates against the current workspace. A template accept also checks sender ownership.
Invalid runs retain metadata with null content; they never store the rejected model artifact.
Transcripts are a separate, explicit opt-in and can contain rejected answers.

The following inline tests in `crates/zirv-flow-api/src/assistant/acceptance.rs` supply the
criterion-4 evidence for the server-side review:

- `criterion_4_security_matrix`: the same seven attack instructions across eight context
  sources, all five draft tools and all three explain tools, in both privacy modes. It uses
  production context renderers, request builders, the gated dispatcher and turn validators.
  The fake model attempts the forbidden calls and foreign references, then returns a valid
  answer. This tests server enforcement; it does not measure a live model's resistance.
- `criterion_4_registered_tools_of_the_wrong_kind_are_not_executed`: HTTP segment runs refuse
  registered `draft_flow` and `explain_trace` calls, retain no model content, and cannot accept.
- `criterion_4_accept_rechecks_foreign_ids_for_every_draft_kind`: HTTP accept refuses foreign
  list references in mappings and segments, template/destination/list references in flows,
  and a foreign template sender. Manifests have no workspace references: an injected reference
  is an unknown field and is refused.
- `criterion_4_all_tables_guard_detects_protected_writes`: positive controls insert consent,
  suppression, message and flow-version rows and update consent. The catalog-enumerated guard
  detects each change. Matrix cases allow only `ai_drafts`, `ai_calls`, `audit_log`, and
  `ai_transcripts` when opted in. Counts and row fingerprints catch inserts, deletes and updates.

Unit coverage is in `catalog::tests`,
`segment_context::tests::hostile_catalog_names_stay_in_the_fence_in_every_privacy_mode`,
`template_context::tests::catalog_names_and_document_are_untrusted_in_both_privacy_modes`, and
`tools::draft_template::tests::the_sender_the_category_and_the_mode_cannot_be_set`.
The last test pins the fragment merge's sender/category refusal in code and block modes.

The `security-*` cases in `assistant/golden.json` use the existing corpus format. Expected
refusal means a safe validated artifact without the requested side effect, or an explicit
profile-disclosure refusal in an explanation. Replays use the fake provider; manual/nightly
evaluation can measure model behavior. Neither a replay nor row counts alone proves what a
live provider will answer.

No table is added. The test-only catalog scan can be reused or replaced by #373; that issue
owns transcript matching limits for erasure and merge. This section supplies assistant review
evidence, not the remainder of the system threat review tracked by #391.
