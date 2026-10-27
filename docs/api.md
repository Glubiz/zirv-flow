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
  for sessions;
- the pagination shape (decided in phase B);
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
verbatim or falls back to a neutral rendering. The server may add values at any time, so the
generated client in `web/` is checked for this (how is decided in phase B). Request enums are the
other direction: removing a value a client may send is breaking; adding one is not.

## Unfrozen surface

These may change in any release, 1.x included:

- `/hooks/{id}`: inbound webhooks, shaped by the sender's payload and signature scheme;
- `/t/*`, `/u/*` and `/a/*`: tracking links, unsubscribe pages and image assets, addressed by signed
  tokens and content hashes, not by a contract;
- `/api/v1/admin/jobs` and its sub-paths: operator tooling for the job queue;
- the `/__test` routes of the browser-test binary;
- the audit export line format (NDJSON and CSV) until it carries a format version (decided in phase B);
- any operation or schema the document tags `preview` (how a preview is marked is decided in phase B).
  None is tagged today.

`/healthz` and `/readyz` are probes outside `/api/v1`. They are frozen like any other operation.

## Versioning

`info.version` in the document stays `"1"` for the whole `/api/v1` series. `GET /api/v1/version`
reports the crate version of the running server, which is how a client tells releases apart.

## Deprecation

A deprecated operation, parameter or field stays for at least one minor release after the 1.0
release that deprecates it. While it stays it is marked `deprecated: true` in the document, and the
operation answers with a `Deprecation` header and a `Sunset` header carrying the earliest removal
date. Removal happens in `/api/v2`, not in `/api/v1`. No operation is deprecated today; the helper
that adds the headers lands with the first deprecation (decided in phase B).

## Idempotency

Creating `POST`s accept an `Idempotency-Key` header (1 to 255 characters). A repeated request with the
same key and body replays the first response for 24 hours; the same key with another body answers
`422` with the code `idempotency_key_reused`. Which operations count as creating is decided in
phase B; the audit lists the candidates.

## Problem codes

Every error is `application/problem+json`. A failure a client can act on carries a stable
snake_case `code`. The list of codes in use, and the rule for which statuses need one, are decided in
phase B; a test then holds the list and the codes in `problem.rs` together.

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
