/**
 * The words for the send pipeline (architecture 10.1): its checks in the order the send job runs
 * them, and the codes a message ends with. One map, so the message log and the trace tree say the
 * same thing. A code the map does not know is shown as it is.
 */

export interface SendGate {
  code: string;
  label: string;
}

/** The checks of a send, in the order the send job runs them. */
export const SEND_GATES: readonly SendGate[] = [
  { code: "enrollment", label: "Enrollment active" },
  { code: "template", label: "Template" },
  { code: "sender", label: "Sender" },
  { code: "address", label: "Address" },
  { code: "consent", label: "Consent" },
  { code: "suppression", label: "Suppression" },
  { code: "quiet_hours", label: "Quiet hours" },
  { code: "frequency_cap", label: "Frequency cap" },
  { code: "render", label: "Render" },
  { code: "rate_limit", label: "Rate limit" },
  { code: "delivery", label: "Deliver" },
];

export function gateLabel(code: string): string {
  return SEND_GATES.find((gate) => gate.code === code)?.label ?? code;
}

const REASONS: Record<string, string> = {
  no_consent: "There is no consent to send this message",
  suppressed: "The address is suppressed",
  frequency_cap: "A frequency cap was reached",
  no_address: "The person has no address to send to",
  sender_unverified: "The sender identity is not verified",
  flow_paused: "The flow is paused",
  flow_inactive: "The flow is no longer active",
  enrollment_ended: "The person left the flow before the message went out",
  template_unavailable: "The template is not available to send",
  render_error: "The message could not be built from its template",
  rate_limited: "The destination kept its rate limit for too long",
  destination_unavailable: "The destination is not available",
  destination_unsupported: "The destination cannot send email",
  delivery_unconfirmed: "The destination may have accepted the message, but its answer was lost",
  address_changed: "The address changed after a send that may have gone out",
  hard_bounce: "The address cannot receive mail (hard bounce)",
  soft_bounce: "The mail server kept refusing the message for now (soft bounce)",
  smtp_4xx: "The mail server asked to try again later and kept doing so",
  internal_error: "The message was never sent because of an internal error",
};

/** A skip or failure code in plain words. */
export function reasonLabel(code: string): string {
  return REASONS[code] ?? code.replaceAll("_", " ");
}

const WAITS: Record<string, string> = {
  quiet_hours: "quiet hours",
  frequency_cap: "frequency cap",
  rate_limit: "rate limit",
};

/** Why a message waits, in words. */
export function waitLabel(why: string | null | undefined): string {
  return why ? (WAITS[why] ?? why.replaceAll("_", " ")) : "";
}

/** What decided the consent check when it is not a rule of the consent policy. */
const BUILT_IN_RULES: Record<string, string> = {
  recorded_consent: "The recorded consent of the person decided.",
  topic_default: "No consent was recorded, so the topic's default decided.",
  transactional: "A transactional message does not need marketing consent.",
  unknown_scope: "The topic or dimension was archived after the flow was published, so consent cannot be read.",
  suppression_workspace: "The address is suppressed in this workspace.",
  suppression_organization: "The address is suppressed in the whole organization.",
};

export function isBuiltInRule(rule: string | null | undefined): boolean {
  return rule !== null && rule !== undefined && rule in BUILT_IN_RULES;
}

export function builtInRuleText(rule: string): string | null {
  return BUILT_IN_RULES[rule] ?? null;
}
