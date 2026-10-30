# Flows

A flow starts a run for a person, follows connected steps, and records what
happened. Each run is an enrollment. Start with [Getting started](getting-started.md)
for a first newsletter; use this page to choose settings and operate a flow.

## Drafts and access

Open **Flows** at `/w/:ws/flows`. **New flow** asks for a name and **Starts when**.
The flow opens at `/w/:ws/flows/:id` in **Build**. Switch to **Test** or **Results**
in the header; the address keeps the view as `?view=test` or `?view=results`.
**Versions** in the header opens the published versions, each read-only.
Changes save to the draft. Wait for **Saved**; if a save fails, use **Retry**
before publishing. A live flow shows **Draft with changes** when its graph or
settings differ from its current published version.

Build has a step palette, the canvas, and a docked inspector. Select a step to
edit its fields without covering the canvas. Select the trigger, or close the
step inspector, to see **Flow settings**. **This step's results** shows counts
for the published version; a new draft step has no published results yet.
**Show counts from version** adds the version's counts to the canvas.

An editor can create and edit drafts and use **Test**. A publisher can also
publish, pause, resume, archive, retry or skip failed steps, and end enrollments.
Admins and the instance owner can do both. Viewers can read. Admins manage the
sources, destinations and credentials a flow uses. The membership grant
`view_pii` is separate from the role: an editor or publisher needs it to test
with a real person. Owners and admins already have access to those values.

**Common mistake:** expecting a saved edit to change the live flow.
**Fix:** review and publish a new version when the draft is ready.

## Triggers

Choose **Starts when** in **Flow settings → Trigger**:

- **A person enters or leaves a segment** (`segment`). Choose **Segment** and
  **When a person**: **Enters the segment** or **Leaves the segment**. **Also
  enroll the people already in the segment when the flow is published** adds
  the current members at publish time. Leave it clear to use changes only.
- **An event happens** (`event`). Choose **Event** and any property filters.
  Matching incoming events can enroll their person; this does not replay the
  event history. For email opens, the picker starts with a filter excluding
  machine opens; keep it if you mean a person's open.
- **On a schedule** (`schedule`). Set **Cron expression**, **Time zone**, and
  **Enrolls** to **Everyone** or **The members of a segment**. For the latter,
  choose **Audience segment**. The cron has five fields: minute, hour, day of
  month, month and day of week. `0 9 * * 1` means Monday at 09:00 in the trigger's
  time zone, such as `Europe/Copenhagen`.
- **By hand** (`manual`). An operator starts the current audience of a segment
  or list through `POST /api/v1/workspaces/{ws}/flows/{id}/start`, supplying
  either `segment_id` or `list_id`. This needs a publisher or a key with
  `publish`. The current flow editor has no audience-start form.
- **From the API** (`api`). An integration calls
  `POST /api/v1/workspaces/{ws}/flows/{id}/enrollments` with either `person_id`
  or `identifier` (its `kind` and `value`). An optional `idempotency_key`
  prevents a retry from enrolling that person twice for the same request.
  This needs an editor or a key with `write`. The person must already exist.

Segment changes caused by editing the segment definition do not trigger flows.
The two API calls queue enrollment work and return 202; the re-entry rule can
still refuse the person. Each call requires its matching published trigger.
Paused flows can accept the request but skip enrollment if still paused when
the work runs. API key scopes are
independent; `write` does not grant publishing. A session-authenticated POST
also needs its CSRF token.

**Common mistake:** expecting existing segment members to enter a change-only
trigger. **Fix:** select the current-members option before publishing, or cause
a new qualifying membership change after publication.

## Re-entry

In **Flow settings → Re-entry**, choose what happens to **A person who has been in
the flow**:

- **Never enters again** (`once`): one entry, even after it ends.
- **Enters again once the earlier run has ended** (`once_at_a_time`): no
  overlapping active enrollment.
- **Enters again, after a pause** (`always`): set **At least this long since
  the last entry**, up to 365 days. Zero permits another qualifying entry without a delay.
  The interval is measured from entry, not exit. This mode also refuses a new
  entry while an earlier enrollment is active, waiting, held or failed.

Re-entry checks and duplicate-trigger handling happen when enrollment is
applied. A new published version does not erase a person's entry history.

**Common mistake:** expecting the interval to admit someone whose earlier run
is still failed. **Fix:** resolve that enrollment first; a failed run still
counts as in flight.

## Connecting steps

In **Build**, filter the step palette and click a kind, or focus it and press
Enter. The palette groups steps into **Timing**, **Logic** and **Actions**.
With a step selected, the new step is inserted on its first output, named in
the palette, and keeps the continuation when it has an output of its own.
Selecting the trigger inserts before the first step. With nothing selected,
or after a step with no output, the new step starts disconnected. Inserting an
Exit ends that path; reconnect any steps left behind before publishing.
The new step opens in the docked inspector.
Under **Where it goes next**, choose a target for each output. A single-path
step has **Next step**. The trigger leads to the one step with no incoming
connection. Every output needs a connection, and every step must be reachable.
Loops are refused. End a path with Exit or a Goal that ends the enrollment.

The following headings name every node type, with the UI name first and its
stored type in parentheses.

**Common mistake:** placing cards next to each other without connecting them.
**Fix:** use the output selectors and resolve the canvas validation problems.

## Wait (`wait.duration`)

Set **Wait for** to a duration from one second to 365 days. The timer starts
when the person reaches the step. Connect **Next step**.

Waits are durable. Restarting a worker does not reset the deadline. A due timer
queues work; worker availability can make the actual continuation later.

**Common mistake:** using a duration to mean a local time tomorrow.
**Fix:** use **Wait until a time** when the person's local clock matters.

## Wait until a time (`wait.until_time`)

Under **Wait until**, choose **The next time of day** with **Time of day** and
optional **On these days**, or **A date and time** with **Date and time**. With
no weekdays selected, every day counts. Connect **Next step**.

The wait uses the person's `timezone` trait, falling back to the workspace
time zone when absent or invalid. An invalid value leaves a fallback note.
A time of day is the next occurrence strictly after now; a date already past
is due immediately. During a daylight-saving gap, a missing local time uses
the earlier offset: 02:30 in a 02:00-to-03:00 gap becomes 03:30. A repeated
local time uses the first occurrence.

**Common mistake:** assuming a wait uses the schedule trigger's time zone.
**Fix:** check the person's and workspace's time zones; a schedule has its own.

## Wait for an event (`wait.until_event`)

Choose **Event**, property filters and **Give up waiting after** (one second to
365 days). Connect both **Arrived leads to** and **Timed out leads to**.
An event arriving for that person while the wait is open can release it.
The timeout takes the other path if no qualifying event releases it first.

**Common mistake:** expecting an event already in the person's history to
release a new wait. **Fix:** use a Branch to check history before waiting for
the next event.

## Branch (`branch`)

Build one to ten conditions with the same builder as a segment. Conditions can
combine **All of**, **Any of** and **Not**, and check a **Field**, **Event**,
**In a list**, **In a segment**, **Consent** or **Has an identifier**. Event
conditions support property filters, time windows and summaries.

The first matching condition wins. One condition gives **Yes** and **No**;
several give **Case 1**, **Case 2** and so on, with **Else** for no match.
Connect every output. The check reads the person's data when the step runs.

**Common mistake:** putting a broad condition before a specific one it includes.
**Fix:** put the specific case first, or make the cases disjoint.

## Random split (`split.random`)

Use two to ten arms. Set **Weight of arm 1** and the other weights to positive
whole numbers that total 100, or choose **Split evenly**. Connect every arm.
The choice is stable for the enrollment and step, so retrying the step does
not make a new draw.

**Common mistake:** expecting a 50/50 split to divide a small audience exactly
in half. **Fix:** treat the weights as chances, not quotas.

## Webhook (`action.webhook`)

Set **URL** and **Traits sent with each call**. Optionally choose a published
webhook **Payload template** to shape the headers and body. Without a template,
the call sends the standard signed JSON envelope: flow, version, step,
enrollment, person and trace ids, an idempotency key, time, and chosen traits.
Connect **Next step**. The result produces `webhook.succeeded` or
`webhook.failed` events.

The receiver must tolerate retries. Outbound URL and address checks can refuse
a destination even when the browser can reach it.

**Common mistake:** sending every profile field when the receiver needs one.
**Fix:** select only the trait keys required by the receiver.

## Send an email (`action.send`)

Choose a published email **Template**, a **Destination** that can send, and
optionally **Topic** and **Consent dimension values**. **No topic** checks
consent without a topic; it does not bypass consent. Connect **Next step**.

The send worker reads the newest published template when it handles the
message. A template fix can therefore reach later sends without a new flow version.
Queuing a message does not pin the template version.
Sending checks the current sender, address and [send gates](#send-gates).
A skipped message continues the flow; a failed delivery can leave a failed step.

**Common mistake:** assuming a template draft is what the flow sends.
**Fix:** publish the template and inspect the message's version and result.

## Sync a profile (`action.sync`)

Choose a provider **Destination** that supports profile sync. Set **Operation**
to **Update the profile** or **Remove the profile**. An update sends the
selected **Traits to send** under the provider's external key. Removal sends
no traits. Connect **Next step**.

**Common mistake:** expecting an SMTP destination to store profiles.
**Fix:** use a provider destination that declares profile-sync support.

## Update a trait (`action.update_trait`)

Choose **Trait** and **Change**: **Set to a value** with **Value**, **Add to a
number** with **Add**, or **Clear what this flow set**. The value must fit the
registered trait's type; adding needs a numeric trait. Connect **Next step**.
The flow writes its own contribution to the field.

**Common mistake:** expecting clear to delete another source's value.
**Fix:** change the source contribution responsible for that value; clearing
this flow's contribution may reveal a value from another source.

## Update a list (`action.list`)

Choose **Change** as **Add the person to** or **Remove the person from**, and choose
**List**. Connect **Next step**. This changes static list membership.

**Common mistake:** expecting list removal to remove the person from a segment.
**Fix:** check the segment definition; it follows its own conditions.

## Goal (`goal`)

A person reaching this step records a conversion. Select **Also end the flow
for the person** to end the enrollment too; otherwise connect **Next step**. This step does not
wait for the optional goal event in Flow settings.

**Common mistake:** placing a Goal before the action that counts as success.
**Fix:** put it on the success path, or use a goal event in Flow settings.

## Exit (`exit`)

This ends the person's enrollment. It has no settings or outgoing connection.
It does not record a goal conversion by itself.

**Common mistake:** trying to continue from Exit.
**Fix:** put further steps before it, or end each branch separately.

## Goals and exit criteria

In **Flow settings → Goal**, **The flow has succeeded when** can be **There is no
goal**, **An event happens**, or **The person enters a segment**. Set **Goal
event** and its filters, or **Goal segment**. **End the flow for a person who
reaches the goal** records the conversion and ends their enrollment. With that
option clear, conversion alone does not stop the remaining steps.

In **Exit criteria**, choose **Add an exit rule**. **Leaves when** can be
**They leave the trigger segment**, **An event happens**, or **They reach the
goal**. An event rule takes an event and property filters. Any matching rule
ends an active enrollment. There can be up to ten rules. Leaving the trigger
segment needs a segment trigger; reaching the goal needs a goal configured.

**Common mistake:** assuming everyone who stops the flow converted.
**Fix:** read goal conversions separately from exits and completion.

## Test mode

Choose **Test** on the flow. Select a real person, or **A made-up person, from
an editable snapshot**, then **Start the test**. A real-person test requires
edit access and `view_pii`; a synthetic snapshot does not require that grant.
An empty snapshot `{}` is enough for a simple send-to-exit path.

The walk uses a virtual clock. Use **Advance the clock…** to pass a wait and
**Send an event…** to inject an event into this test. Read the highlighted path,
branch results and captured effects in the timeline on the right. The controls
stay on the left; untaken branches are dimmed on the canvas. Use **Start over**
for another scenario and **Build** to return to editing. Save settings first: the canvas graph
can be tested unsaved, but the walk reads goal and exit settings from the saved
draft.

It does not send email, render templates, call webhooks or sync destinations.
It does not evaluate send gates. Trait and list changes affect only the test's
snapshot. It creates no enrollment, job, event or decision record; the request
does write an audit entry. Test runs never count in **Results**. It walks one person directly and does not prove
trigger enrollment or re-entry behavior. Segment membership changes are not
scripted, so segment goals and segment-driven exits are listed as not simulated.

Use a template's **Send a test** to inspect an actual captured email. That is a
separate action which sends to allowlisted test inboxes. Test mode is not a
sandbox switch for a published flow: its live enrollments can still run.

**Common mistake:** reading a captured send effect as a delivered message.
**Fix:** test the template's rendering and delivery separately, then inspect
the live send gates and SMTP capture for your synthetic enrollment.

## Publishing and running versions

Choose **Review and publish**, which shows the issue count and is disabled
while validation reports blocking issues. Problems appear on their nodes;
use the canvas issue list to open the affected field or settings section.
The review checks the saved draft and shows the next
version, entry estimate, older enrollments and input warnings. Resolve errors,
then choose **Publish version** with the displayed number. A segment entry
estimate is available when current members will be enrolled; it is not a
promise of that many sends.

New enrollments use the newly published version. Existing enrollments keep the
version they entered until they finish. Editing or publishing does not migrate
them. In **Results**, select an older version under **Counts for** to review its remaining
people; a publisher can choose **End their enrollments** after confirmation.
Ending them does not enroll them again, and re-entry rules still apply.

Publishing a paused flow keeps it paused. A flow version pins its graph and
settings, but a Send an email step resolves the current published template
when the send worker handles a message.

**Common mistake:** publishing a fix and assuming everyone waiting now uses it.
**Fix:** inspect their version and decide whether to let it finish or end those
enrollments. Publish before relying on the fix for new entries.

## Results and traces

Open **Results** and choose **Counts for**. Totals show step runs, waiting
enrollments, messages sent and goal conversions for that version. The step
table shows reached (step runs), waiting, failed step runs, skipped messages
and delivery outcomes. A retried step counts again: reached is not a count of
unique people. These are cumulative counts computed when opened, without a
date range or daily chart. A failed count request shows an error, not zeros.

Below the table, filter **Enrollments** by **State**, **Version** or **Step**,
then open one enrollment. Its panel shows the path on that enrollment's graph, the executed steps, and a
**Trace** tree. Read the decision's action, outcome and reason together.
The person lookup links to that person's **Messages** view to investigate sends
and skipped messages even when you have not selected an enrollment.

The tree uses labels such as **A person was considered for a flow**
(`flow.enroll`), **A branch chose a path** (`flow.branch`), **A flow step ran**
(`flow.step`) and **A waiting enrollment woke up** (`flow.wake`). Send outcomes
include **Message sent**, **Message skipped**, **Message failed** and **Message
held for later** (`message.send`). The `send.gate` records name the checks that
passed or stopped the message. Job attempts and operator decisions explain
retries and skips.

**Open in Traces** opens `/w/:ws/traces/:traceId`. This page shows the raw
records in a table; the tree is in the enrollment inspector. It shows a
truncation notice when the trace exceeds the first 1,000 records. Decision
records use ids and fixed codes, not email addresses, personal trait values or
event properties. Use the linked profile and message history for the permitted
person-level detail.

> **Screenshot pending:** capture one enrollment path and its Trace tree from
> the browser harness with synthetic data. Store the approved lean view under
> `docs/guide/img/`. Do not replace the instructions with the image.

**Common mistake:** treating a completed enrollment as proof that every send
went out. **Fix:** check each message outcome; a skipped send can continue to Exit.

## Retry and skip a failed step

Open a failed enrollment under **Results → Enrollments** and read its failing step and trace.
Correct the cause first. A publisher can choose **Retry** (the button also names
the failed step) to run it again, or **Skip it…**, then **Skip the step**, to
continue without running it. These actions are recorded on the enrollment trace.

The flow must be published and running; resume a paused flow first. Archived
flows cannot retry or skip. A send skipped by a gate is different from a failed
step: retry and skip are operator actions for failed enrollments.
External calls may already have reached their receiver before a failure is
known; inspect the attempt and receiver before retrying.

**Common mistake:** using Skip to fix a missing required action.
**Fix:** repair the cause and retry when the action must happen; skip only when
continuing without it is intended.

## Pause, resume and archive

**Pause** stops new enrollments and holds execution as steps observe the pause.
It is not an undo for an external call already in progress. **Resume** lets
held enrollments continue. Time still passes during a pause; resuming does not
restart every duration. Trigger occurrences skipped while paused are not a
backlog of people waiting to enter.

**Archive…**, then **Archive flow**, is permanent. No new person enters and
remaining enrollments are exited in batches. The versions and history stay
readable. An archived flow cannot be published or resumed.

**Common mistake:** archiving a flow for a short interruption.
**Fix:** use Pause and Resume when you plan to continue it.

## Send gates

Before delivery, the send pipeline checks the sender, recipient address,
consent, suppressions, frequency caps, quiet hours, rendering and destination
rate. These decisions appear as `send.gate`. A successful flow test does not
check them. Admins open **Sending**, headed **Sending limits**, at
`/w/:ws/settings/sending`.

- **Consent and suppression.** A marketing message uses its channel, topic and
  dimension values, recorded consent and any published consent policy.
  Missing permission can skip it with `no_consent`. Suppression can skip it
  with `suppressed`. A transactional template bypasses marketing consent, but
  still checks suppression. Transactional messages also skip frequency caps
  and quiet hours. Do not label a newsletter transactional to bypass
  an opt-out.
- **Frequency caps.** A cap limits messages to a person on a channel over its
  time window, across flows. The cap can skip the message or hold it until a
  slot is available. A skipped cap records `frequency_cap`; a deferral is
  **Message held for later**.
- **Quiet hours.** A message inside the configured quiet period waits until
  its end, using the recipient's time zone with a workspace fallback. It gives
  up its cap slot while waiting. This is a deferral, not a skipped send.
- **Sender, rendering and destination.** The sender must be usable on a
  verified domain and the person must have an address. A rendering failure
  records `render_error`. Marketing links need the configuration key
  `server.public_url`. A destination rate limit can hold work for later.

A deferred message is checked again when it resumes before delivery. A retry
of a delivery that may already have gone out follows the recorded delivery
attempt rather than treating it as a new message with fresh person gates.

**Common mistake:** raising limits because an enrollment has not advanced yet.
**Fix:** read the message outcome and gate reason first. A quiet-hours deferral,
consent skip and transport failure need different changes.
