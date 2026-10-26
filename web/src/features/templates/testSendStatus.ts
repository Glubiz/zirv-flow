import type { Tone } from "../../ui";
import type { TestSend } from "./model";

export interface TestSendStatus {
  label: string;
  tone: Tone;
  /** Plain words for what happened; empty when there is nothing to add. */
  detail: string;
  /** Still on its way, so the log keeps asking. */
  pending: boolean;
}

const REASONS: Record<string, string> = {
  abandoned: "The send was lost before it ran. Send it again.",
  credential_unavailable: "The destination's credential could not be used.",
  delivery_unconfirmed: "The destination did not confirm. It was not retried, so it may or may not arrive.",
  destination_unavailable: "The destination is archived or missing.",
  destination_unsupported: "The destination cannot send email.",
  internal_error: "Something went wrong on our side. Send it again.",
  person_unavailable: "The person whose values fill the fields no longer exists.",
  rate_limited: "The destination's sending rate was reached.",
  recipient_unavailable: "The recipient is no longer a member or on the allowlist.",
  sender_unverified: "The sender's domain is not verified.",
  soft_bounce: "The destination could not deliver it for now.",
  suppressed: "The address is on the suppression list. Nothing was sent.",
  template_unavailable: "The template or its version is archived or gone.",
};

const FALLBACKS: Record<string, string> = {
  failed: "The destination did not take it.",
  render_failed: "The template could not be rendered.",
  sender_unverified: "The sender's domain is not verified.",
};

/** The word for a log row: the outcome, and for a failure the reason in plain words. */
export function statusOf(row: Pick<TestSend, "outcome" | "reason">): TestSendStatus {
  const reason = row.reason ? REASONS[row.reason] : undefined;
  switch (row.outcome) {
    case "queued":
      return { label: "Queued", tone: "neutral", detail: "Waiting for a send slot.", pending: true };
    case "sent":
      return { label: "Sent", tone: "neutral", detail: "", pending: false };
    case "suppressed":
      return { label: "Suppressed", tone: "attention", detail: reason ?? REASONS["suppressed"] ?? "", pending: false };
    case "unconfirmed":
      return { label: "No answer", tone: "attention", detail: REASONS["delivery_unconfirmed"] ?? "", pending: false };
    case "failed":
    case "render_failed":
    case "sender_unverified":
      return { label: "Failed", tone: "error", detail: reason ?? FALLBACKS[row.outcome] ?? "", pending: false };
    default:
      return { label: row.outcome, tone: "neutral", detail: "", pending: false };
  }
}
