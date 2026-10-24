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
 * What a send decision says in plain words, or none for a decision this map does not know. The
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
      return MAIL_EVENTS[record.action] ? `${MAIL_EVENTS[record.action]}${record.reason === "hard" || record.reason === "soft" ? ` (${record.reason})` : ""}` : null;
  }
}
