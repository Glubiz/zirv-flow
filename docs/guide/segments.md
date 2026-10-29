# Segments and lists

Use a segment when membership should follow rules; use a list when you want an
explicit set of existing people. Saving a segment changes its current definition.
It does not send a message: a [flow](flows.md) decides what happens on entry or exit.

## Worked example: repeat customers

Assume `country` is a registered string field and `order_placed` has a published
schema with a numeric `total` property. Open **Segments** and create a segment:

1. Use an **All of** group. Add a field condition: country **is one of** DK, SE, NO.
2. Add an event condition for `order_placed`. Filter its `total` to **is at least**
   50, choose **Number of events**, **is at least** 2, and **90 days**.
3. Inspect the matching count and data age, then save. Use **Refresh now** if you
   need to queue a full membership update immediately.
4. Open a person and explain this segment. Check each condition and compare the
   fresh result with the stored membership; an update may still be pending.

The definition for the same audience is:

```json
{"v":1,"all":[
  {"trait":"country","op":"in","value":["DK","SE","NO"]},
  {"event":"order_placed","where":[{"prop":"total","op":"gte","value":50}],
   "agg":{"fn":"count","op":"gte","value":2},"window":{"last":"P90D"}}
]}
```

## Rules and event aggregates

The builder combines **All of**, **Any of** and **Not** groups with field, event, list,
segment, consent and identifier conditions. Field and property operators follow
the registered type. References to other segments cannot form a cycle.

Missing values matter: a missing trait or missing/null event property makes a
comparison false, including `neq` and `not_in`. Only `not_exists` matches absence.
Negating a condition is different: `not (country eq DK)` includes a person with
no country, while `country neq DK` excludes them.

Event filters select events before aggregation. Available aggregates are count,
sum, minimum, maximum, average, first time and last time. With no matching events,
count is zero; other aggregates are missing and comparisons with them are false.
Use count equal to zero for “no orders in the window”. A window can be a relative
duration, a fixed interval or start at a person's timestamp trait; without one,
all retained matching events are considered. Date comparisons use UTC midnight.

A 90-day aggregate reads retained raw events, rather than a daily rollup. It
costs more than a simple field filter, especially with several event conditions,
property filters or a cold cache. The [M4 report](../benchmarks/m4-segments.md)
records a 4.68 s aggregate-preview p95 and a 15.1 s cold-cache observation on its
historical development dataset; these are not a latency guarantee for your data.
Narrow the event name and window where the audience permits it, and inspect data
age before deciding the count is useful.

## Segments

A segment is a named, versioned definition of a group of people, built from traits, events,
lists, consent and other segments. The definition format is the AST in the OpenAPI document
(`SegmentDefinition`). You can save and preview, and a `segment.refresh` job computes the
members: `GET .../segments/{id}/members` and `.../history` read them, and
`POST .../segments/{id}/refresh` (an editor or a `write` key) queues a refresh now (202).

- **Create and version.** `POST /api/v1/workspaces/{ws}/segments` with `{name, description?,
  cadence_minutes?, definition}` creates version 1. `PATCH .../segments/{id}` with a
  `definition` saves the next version and makes it current; there is no separate publish, so
  an editor or above (or a `write` key) saves. Versions are immutable: `GET .../segments/{id}/versions`
  and `.../versions/{n}`. `cadence_minutes` is one of 5, 10, 15, 30, 60, 120, 180, 240, 360,
  480, 720, 1440 or 10080 (default 60).
- **Checked at save.** A definition is checked against the workspace's traits, event schemas,
  lists, live segments and consent topics. A failure is 422 `segment_invalid` with
  `issues: [{pointer, code}]`, never a value. A segment that refers to another that refers back
  is 422 `segment_cycle`, with `cycle: [{id, name}]` in path order and a detail such as
  "A → B → A". A reference to a missing or archived segment is `unknown_segment`.
- **Archive.** `POST .../segments/{id}/archive` is refused with 409 `segment_referenced` while a
  current version of another live segment references it. The list hides archived segments
  unless `?archived=true`.
- **Preview.** `POST .../segments/preview` with `{definition, sample_size?}` (20 by default,
  100 at most) runs an unsaved definition on the primary and returns `{count, sample,
  data_age, compiled_at}`. It reads in one read-only snapshot and stops at 20 seconds with 503
  `segment_preview_timeout`. Sample identifiers are masked like the people search. `data_age`
  is the oldest `last_success_at` of the enabled streams whose saved mapping writes a trait,
  event or list the definition reads (including through segments it references), with each
  feeding source; with no feeding stream `as_of` is null and the data is live.
- **Access and audit.** `read` looks and previews; creating, saving and archiving need a
  workspace editor or a `write` key. Writes are audited as `segment.created`,
  `segment.updated` (with the version) and `segment.archived`; the audit never holds a
  definition's values.
- **In the web UI.** Open **Segments** to build a segment from nested all, any and not groups and conditions, watch the matching count update as you edit, save versions, see members and history, and ask on a person's profile why they are or are not in a segment.

## Lists

A list is a named set of people that you fill by hand, from a CSV, through the API or with flow list actions. Open **Lists** in the web UI to create, rename and
delete lists and to see who is in one. Deleting a list asks first, naming the list and how
many people it holds; it removes the list, its members and their membership history, and the
people stay. A list name is unique in a workspace, ignoring case. Editors and above can manage lists; an API key needs the `write` scope to change them and `read` to
look at them.

**Adding people.** Three ways, all of which add only people who already exist and never
create one:

- by id: `POST /api/v1/workspaces/{ws}/lists/{id}/members` with `{"person_ids": [...]}`;
- by identifier: the same call with `{"identifiers": [{"kind": "email", "value": "..."}]}`, or
  the **Add a person** form on the list page. Identifiers are normalized like everywhere
  else, and one that no person owns comes back in `not_found` with its position;
- by CSV: `POST .../lists/{id}/members/import` with `Content-Type: text/csv`, a header row
  `kind,value` and one identifier per row, up to 10 MiB and 100,000 rows. Add
  `?dry_run=true` to see `{matched, added, already_member, unmatched}` without changing
  anything; the web UI always previews first. `unmatched` lists the line of the file each skipped row
  starts on (the header is line 1, as an editor shows it; the field is named `row`) with a
  reason code and never echoes a value.

Remove people with `POST .../lists/{id}/members/remove` and `{"person_ids": [...]}`, or tick
them on the list page. One call takes at most 1,000 people.

**Idempotent.** Adding someone who is already in the list, or removing someone who is not,
changes nothing and writes nothing. The member count on a list is exact: it is kept in the
same transaction as every change. A person id that was merged into another is read as the
person it was merged into.

**History.** Every real change writes a row of the person's membership history (list, added
or removed, source, who and the trace) and a `list.membership` decision that holds ids only.
Read it with `GET /api/v1/workspaces/{ws}/people/{id}/lists` (the lists a person is in) and
`GET .../people/{id}/lists/history`. Creating, renaming and deleting a list is audited as
`list.created`, `list.updated` and `list.deleted`; membership changes are not audited.

**Masking.** The member page shows each person's strongest identifier. The owner, workspace admins and `admin` keys see the value, as do memberships with
`view_pii` and keys with `pii`. Other readers see its kind and a `masked` flag instead.

**Merges.** When two people are merged, the survivor is in every list either of them was in,
keeps the earlier of the two `added_at` times, and the counts follow. The merged person's
history moves to the survivor.

## Segment assistant

On a segment (new or saved), the web UI offers assistant drafting and
**Explain this segment** to admins when the assistant is enabled. The draft and
accept API also allows editor sessions; AI explanation remains admin-only.
API keys cannot call the assistant. See [assistant settings and privacy](templates.md#ai-assistant).

- **Draft.** Say who should belong. The model calls `draft_segment` with a whole definition, which
  is checked with the rules a builder save applies (the parser, the validator against the
  workspace's catalog and the compiler). Problems go back to it as a code and a JSON pointer, never
  a value, at most `ai.max_repair_attempts` times. A valid draft is shown as a difference from the
  saved version with the live count of the draft. **Accept** loads it into the builder as the
  unsaved copy of that segment, after a dialog if you had unsaved edits to the conditions; **Save**
  publishes it, as always. A segment draft names the segment (or none, for a new one), has no
  source, and stores the SHA-256 of the saved version it was made from.
- **Explain.** `POST /api/v1/workspaces/{ws}/segments/{id}/ai/explain` (optional body
  `{"version": n}`, default the current version) answers `{version, summary, nodes: [{pointer,
  text}]}`. The model calls `explain_segment`; every pointer must be a node of that version, every
  condition must be cited exactly once and the texts are plain text (500 characters per node, 1,000
  for the summary). Nothing is stored. The page shows each condition in the builder's own words
  next to the sentence. 404 for an unknown segment or version, 422 `ai_explain_failed` (with issue
  codes) when the attempts run out.
- **Privacy.** The model is told trait keys, types and descriptions, event names with property
  names and types, list and segment names, consent topics and identifier kinds. It is never told a
  trait or event value, a count, a sample or a member. With the personal-data opt-in off, every
  string literal in the saved definition that is compared with a trait or event property that is
  not declared non-personal (or is not defined) is replaced by a placeholder such as `(kept:1)`
  and put back in the draft, so the model keeps it by leaving the placeholder where it is. A
  placeholder moved to another condition, or made up, is a problem it must fix. With the opt-in the
  definition is sent as it is, fenced as personal data.
- **Accept.** `POST .../ai/drafts/{id}/accept` locks the segments of the workspace the way a save
  does, answers 409 `ai_draft_outdated` when the current version is no longer the one the draft was
  made from, and 409 `ai_draft_invalid` with the issues when the definition no longer validates
  (for example, a field archived since). It writes no segment version and is audited as
  `ai.draft.accepted` with the kind and the segment.
- **Roles.** Drafting and accepting require an editor or above; AI explanation requires
  an admin. Both need a session and an enabled assistant. An API key is refused with
  `ai_user_required`. Saving an accepted segment draft remains a separate write.

## Explain and membership updates

`GET /api/v1/workspaces/{ws}/people/{id}/explain?segment=<id>` evaluates the saved
definition for one person and returns a pass/fail tree, observed values subject
to personal-data permissions, and stored membership. It uses a read-only snapshot
with the same 20-second ceiling as preview. It loads at most 10,000 relevant events;
conditions affected by that cap say unknown, with reason `capped`, rather than
inventing a pass or fail. A timeout returns 503 `explain_timeout`.

Data changes mark affected people for incremental re-evaluation. The worker checks
segments whose dependencies changed, then dependent segments in dependency order.
Scheduled full refreshes handle time passing without new data, such as an order
aging out of a 90-day window. Saving a definition also queues a refresh; the default
cadence is hourly. **Refresh now** queues work, it does not synchronously finish it.

Membership changes keep history and emit `segment.entered` or `segment.exited`.
Definition changes are recorded, but flow entry/exit triggers ignore changes caused
by editing a definition. To reach existing members, use the flow's publish options
or start it for an audience; see the [flows guide](flows.md).

## Limits

- Preview samples default to 20 people and stop at 100; the count is not limited
  to the sample. Preview, member reads and explain use the primary database and
  have a fixed 20-second statement timeout, with no configuration override.
- A preview evaluates now; saved membership can lag while refresh work is queued.
  Relative-time rules need scheduled refreshes even when no new records arrive.
- Events outside retention cannot contribute. A capped explanation may be unknown.
- Numeric comparisons use floating point; integers above 2^53 lose precision,
  and aggregate equality can depend on summation order.
- List imports match existing people only; they do not create contacts. Removing
  a list deletes its membership history, but leaves the people.
