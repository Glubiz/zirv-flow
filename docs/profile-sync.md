# Keeping people in an external system

Profile sync keeps a person's profile in another system (a contact store, a CRM-like
service, anything with an HTTP API) in step with what Zirv Flow knows. It is not a second
mechanism: it is a flow. An `action.sync` step either **upserts** the profile with a list of
traits, or **removes** it, through a *provider destination* whose manifest describes the API.

Architecture: section 10.1 and 10.2, "As built (profile sync, #226)".

## Describe the API: the manifest

A provider destination's configuration is a manifest (`config.manifest`, manifest v1) with a
`destination` section. The email half (`send` with `feedback`) and the profile operations are
independent: a manifest declares the email pair, either operation, or any mix, and at least one
of them. A `send` without `feedback` (or the reverse) is refused.

```json
{
  "version": "v1",
  "base_url": "https://api.fixture-provider.example.test",
  "auth": { "type": "bearer" },
  "destination": {
    "upsert_profile": {
      "method": "PUT",
      "path": "/v1/profiles/{key}",
      "idempotency_header": "Idempotency-Key",
      "key_from": { "identity": "email" },
      "body": {
        "key": "/profile/id",
        "traits": "/profile/attributes",
        "static": { "/source": "zirv-flow" }
      },
      "rate_limit": { "requests": 10, "per_seconds": 1 },
      "errors": [{ "status": 409, "action": "retry" }]
    },
    "remove_profile": {
      "method": "DELETE",
      "path": "/v1/profiles/{key}",
      "idempotency_header": "Idempotency-Key",
      "key_from": { "identity": "email" }
    }
  }
}
```

The same file ships as a fixture:
`crates/zirv-flow-connectors/tests/fixtures/providers/profile_sync.json` (a made-up provider).

Each operation is one HTTP request:

| Key | Meaning |
|-----|---------|
| `method` | `POST`, `PUT` or `PATCH` for an upsert (default `POST`); also `DELETE` for a remove (default `DELETE`). |
| `path` | Starts with `/`. It may hold the placeholder `{key}` once; no other placeholder exists. The external key is percent-encoded as one path segment, so a key holding `/`, `?` or `#` cannot change the route; a key of `.` or `..` is refused, and the sync fails with no call. |
| `idempotency_header` | Carries the step's effect key on every attempt, so a retry cannot duplicate. |
| `key_from` | Where the external key comes from: `{"identity": "email"}` (any identifier kind) or `{"trait": "<trait key>"}`. |
| `body.key` | Places the external key at a JSON pointer. The key goes in the path, the body, or both. |
| `body.traits` | Places one object holding the step's listed traits at a JSON pointer. |
| `body.fields` | Instead of `traits`: places single traits at pointers, as `{"plan": "/plan"}`. A listed trait the manifest does not name is not sent. |
| `body.static` | Literal values placed beside the rest. |
| `rate_limit`, `errors` | As for email: the default rate of a destination created without one, and the error rules that classify a response (`retry` or `terminal`; a profile has no recipient, so `recipient_failure` is refused). |

A remove takes no `traits` or `fields`. The manifest holds no secret; the destination's
credential does. The manifest is data: an unknown key, a `{{ }}` marker or a second placeholder
is refused, and every problem is named by its place in the manifest, never by its value.

## Create the destination

Create a destination of type **Email provider (manifest)** (`provider`) with the manifest as its
configuration and a credential for what the manifest's `auth` reads. A manifest of profile
operations only needs no feedback secret. Its rate limit is the destination's rate: every sync
takes a token from the same bucket, and a `429` slows the bucket by its `Retry-After`.

## Keep a segment in the external system

Two flows, one per direction. Both are triggered by the segment:

1. **Entry.** Trigger *A person enters a segment*, with *also enroll the current members* if the
   system should start in step. The flow has one step: `action.sync`, operation `upsert`, listing
   the traits to keep in step.
2. **Exit.** Trigger *A person leaves a segment*. The flow has one step: `action.sync`,
   operation `remove`.

Set re-entry to **Always** on both so a person who leaves and comes back is synced again. The
worked example is `crates/zirv-flow-core/tests/fixtures/flows/segment_sync.json` (the graph and
settings of both flows, with made-up ids); a test validates it.

An upsert sends the traits as they are when the job runs, and a trait the person has no value
for is sent as `null`. To keep the external system current when a trait changes, add an
`action.sync` step after the step that changes it, or run the flow on a schedule.

## What happens when it runs

- The step queues a `delivery.sync` job under the step's effect key. The job reads the person,
  their external key and the listed traits without holding a lock, takes a token from the
  destination's bucket, calls the provider, and only then locks the person to write the outcome.
- A `2xx` succeeds. A `408`, `429`, `5xx`, a timeout or a transport error is retried with
  backoff, or after the provider's `Retry-After` when that is longer. Any other `4xx`, a
  destination the egress policy refuses and a manifest rule marked `terminal` fail at once.
- The outcome is the person event `sync.succeeded` or `sync.failed` (a flow can wait on either)
  and a decision, `sync.upsert` or `sync.remove`, under the enrollment's trace. They hold the
  destination id, the operation, the status, the attempt count and a fixed reason code such as
  `http_4xx`, `missing_key`, `operation_unsupported`, `destination_unavailable` or
  `rate_limited`. **Never** the external key or a trait value.
- A person without the key (for example no identity of the kind the manifest names) ends
  `sync.failed` with `missing_key`, and nothing is sent.
- A person who was erased, who left the flow, or who was merged into another gets no call.
  Erasing a person removes their pending `delivery.sync` jobs. Telling the external system to
  erase them is the destination's erase capability (a separate step).
- Test mode shows the sync (destination, operation, the listed traits at the values of the walk)
  and sends nothing and writes nothing.

## Publish checks

Publishing a flow refuses an `action.sync` step whose destination does not exist, is archived or
is not a provider destination (`sync_destination_unusable`), whose manifest does not declare the
operation (`operation_unsupported`), that lists an unknown trait (`unknown_trait`), more than 50
traits (`too_many_trait_keys`), or traits on a remove (`trait_keys_not_allowed`).
