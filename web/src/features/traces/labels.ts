import { gateLabel, reasonLabel } from "../../ui";

/** The part of a decision record the labels read. */
interface Labelled {
  attempt?: number | null;
  action: string;
  outcome: string;
  reason?: string | null;
  subject_ids: Record<string, unknown>;
}

const SEND_OUTCOMES: Record<string, string> = {
  sent: "Message sent",
  skipped: "Message skipped",
  failed: "Message failed",
  deferred: "Message held for later",
  dropped: "Message dropped before it started",
  noted: "Note on a message",
};

const FEEDBACK: Record<string, string> = {
  delivered: "Delivery confirmed (email.delivered)",
  bounced_hard: "Bounced for good (email.bounced, hard)",
  bounced_soft: "Bounced for now (email.bounced, soft)",
  complained: "The person complained (email.complained)",
};

/** Mail events a decision may be named after: delivery feedback and tracking. */
const MAIL_EVENTS: Record<string, string> = {
  "email.delivered": "Delivery confirmed",
  "email.bounced": "Bounced",
  "email.complained": "The person complained",
  "email.opened": "Opened",
  "email.clicked": "A link was clicked",
  "email.unsubscribed": "Unsubscribed",
};

/**
 * What each decision action means, in plain words. The action list is the registry's, kept in
 * `fixtures/decision-actions.json` by a Rust test, and `labels.test.ts` fails on an action that
 * has no label here.
 */
export const ACTION_LABELS: Record<string, string> = {
  "event.accepted": "An event was accepted",
  "event.rejected": "An event was rejected",
  "record.rejected": "A synced record was rejected",
  "event.replayed": "A rejected event was replayed",
  "person.created": "A person was created",
  "trait.change": "A field changed",
  "contribution.removed": "A source record's data was removed",
  "identity.split": "An identifier moved to a new person",
  "identity.limit": "An identifier was skipped at the limit",
  "identity.conflict": "Two people share an identifier",
  "identity.conflict.resolved": "An identity conflict was resolved",
  "person.merge": "Two people were merged",
  "person.merge.rewrite": "A merge was carried through the history",
  "person.deleted": "A person was deleted",
  "consent.change": "Consent changed",
  "consent_policy.publish": "The consent policy was published",
  "list.membership": "A list membership changed",
  "template.publish": "A template was published",
  "sending_domain.verify": "A sending domain was checked",
  "flow.publish": "A flow was published",
  "flow.pause": "A flow was paused",
  "flow.resume": "A flow was resumed",
  "flow.archive": "A flow was archived",
  "flow.enroll": "A person was considered for a flow",
  "flow.enrollment": "An enrollment changed state",
  "flow.step": "A flow step ran",
  "flow.wake": "A waiting enrollment woke up",
  "flow.wait": "An enrollment started to wait",
  "flow.goal": "A flow goal was reached",
  "flow.branch": "A branch chose a path",
  "flow.branch.condition": "A branch condition was checked",
  "send.gate": "A send check ran",
  "message.send": "A message send was decided",
  "message.feedback": "Delivery feedback was recorded",
  "suppression.feedback": "Feedback asked for a suppression",
  "webhook.call": "A webhook was called",
  "sync.upsert": "A profile was synced to a destination",
  "sync.remove": "A profile was removed from a destination",
  "event.forward": "An event was forwarded",
  "sync.record": "A sync run handled a record",
  "segment.entered": "A person entered a segment",
  "segment.exited": "A person left a segment",
  "segment.refresh": "A segment was refreshed",
  "segment.reevaluate": "Segments were re-evaluated",
  "job.attempt": "A job attempt",
  "job.retry": "A job was retried by an operator",
};

/** The plain words for a decision action, or none for one this table does not know. */
export function actionLabel(action: string): string | null {
  return ACTION_LABELS[action] ?? null;
}

/**
 * A job attempt the worker recorded, with no subject ids to link: it names the attempt, how it
 * ended and why, and nothing it cannot know.
 */
function attemptLabel(record: Labelled): string {
  const number = record.attempt != null ? `Attempt ${record.attempt}` : "Attempt";
  // The last attempt to fail is "exhausted" to the worker; to a reader it failed like the others.
  const how = record.outcome === "exhausted" ? "failed" : record.outcome === "failed" || record.outcome === "succeeded" ? record.outcome : record.outcome.replaceAll("_", " ");
  return record.reason ? `${number} ${how}: ${reasonLabel(record.reason)}` : `${number} ${how}`;
}

/**
 * What a decision says in plain words, or none for an action this map does not know. The
 * record's own codes are shown beside it, so nothing is lost when the words are missing.
 */
export function recordLabel(record: Labelled): string | null {
  switch (record.action) {
    case "message.send": {
      const base = SEND_OUTCOMES[record.outcome];
      if (!base) return null;
      return record.reason && (record.outcome === "skipped" || record.outcome === "failed") ? `${base}: ${reasonLabel(record.reason)}` : base;
    }
    case "send.gate":
      return record.reason ? `${gateLabel(record.reason)} check ${record.outcome === "passed" ? "passed" : "stopped the send"}` : null;
    case "message.feedback":
      return (record.reason ? FEEDBACK[record.reason] : undefined) ?? null;
    case "suppression.feedback":
      return record.outcome === "added" ? "The address was suppressed after the feedback" : "The feedback could not suppress the address";
    case "job.attempt":
      if ("destination" in record.subject_ids) return `Attempt to reach the destination: ${record.outcome.replaceAll("_", " ")}`;
      return attemptLabel(record);
    default:
      return MAIL_EVENTS[record.action] ? `${MAIL_EVENTS[record.action]}${record.reason === "hard" || record.reason === "soft" ? ` (${record.reason})` : ""}` : actionLabel(record.action);
  }
}
