import type { components } from "../../api";
import { builtInRuleText, formatDateTime, gateLabel, isBuiltInRule, reasonLabel, SEND_GATES, waitLabel } from "../../ui";

export type Message = components["schemas"]["MessageBody"];
type GateResult = components["schemas"]["GateResultBody"];

export type GateState = "passed" | "stopped" | "held" | "not_reached";

export interface GateRow {
  code: string;
  label: string;
  state: GateState;
  /** The code the gate stopped the message with, when it did. */
  reason: string | null;
}

/** The last thing each gate recorded, as the send job wrote it. */
function lastByGate(results: readonly GateResult[]): Map<string, GateResult> {
  const last = new Map<string, GateResult>();
  for (const result of results) {
    // A note that a call may be in flight is no verdict: the verdict, if there is one, comes after it.
    if (result.result === "in_flight") continue;
    last.set(result.gate, result);
  }
  return last;
}

/** Every check of the send in the order the job runs them, each passed, stopped here, held or not reached. */
export function gateRows(message: Message): GateRow[] {
  const last = lastByGate(message.gate_results);
  const known = new Set(SEND_GATES.map((gate) => gate.code));
  const extra = [...last.keys()].filter((code) => !known.has(code)).map((code) => ({ code, label: gateLabel(code) }));
  return [...SEND_GATES, ...extra].map(({ code, label }) => {
    const found = last.get(code);
    let state: GateState = "not_reached";
    if (found?.result === "passed") state = "passed";
    else if (found?.result === "deferred") state = message.status === "pending" ? "held" : "passed";
    else if (found) state = "stopped";
    return { code, label, state, reason: state === "stopped" ? (found?.reason ?? null) : null };
  });
}

export type ResultKind = "sent" | "skipped" | "deferred" | "failed" | "waiting" | "sending";

export interface MessageResult {
  kind: ResultKind;
  /** The first words: Sent, Skipped, Deferred until 08:00 (quiet hours), Failed. */
  headline: string;
  /** The code that follows the headline, shown in mono. */
  code: string | null;
  /** The rule the code rests on, such as the consent policy rule that denied the send. */
  rule: string | null;
  /** A line under the result. */
  note: string | null;
}

/** What happened to a message, in the words of the log. */
export function resultOf(message: Message): MessageResult {
  const reason = message.reason ?? null;
  switch (message.status) {
    case "sent":
      return { kind: "sent", headline: "Sent", code: null, rule: null, note: message.completed_at ? `Handed over at ${formatDateTime(message.completed_at)}` : null };
    case "skipped":
      return {
        kind: "skipped",
        headline: "Skipped",
        code: reason,
        rule: reason === "no_consent" || reason === "suppressed" ? (message.deciding_rule ?? null) : null,
        note: reason && reason !== "no_consent" ? reasonLabel(reason) : null,
      };
    case "failed":
      return { kind: "failed", headline: "Failed", code: reason, rule: null, note: reason ? reasonLabel(reason) : null };
    case "pending":
      if (message.deferred_until) {
        const why = waitLabel(message.deferred_why);
        return {
          kind: "deferred",
          headline: `Deferred until ${formatDateTime(message.deferred_until)}${why ? ` (${why})` : ""}`,
          code: null,
          rule: null,
          note: "Every check runs again then.",
        };
      }
      return { kind: "waiting", headline: "Waiting to be sent", code: null, rule: null, note: null };
    default:
      return { kind: "sending", headline: "Being handed to the destination", code: null, rule: null, note: null };
  }
}

/** One or two plain sentences that say why the message has the result it has, from its gate record. */
export function explain(message: Message): string {
  const rows = gateRows(message);
  const stopped = rows.find((row) => row.state === "stopped");
  const after = stopped ? ` The send stopped at the ${stopped.label.toLowerCase()} check and nothing after it ran.` : "";
  const reason = message.reason ?? null;
  switch (message.status) {
    case "sent":
      return `Every check passed and ${message.destination.name ?? "the destination"} accepted the message.`;
    case "skipped": {
      if (reason === "no_consent") {
        const rule = message.deciding_rule ?? null;
        if (rule && !isBuiltInRule(rule)) {
          return `The consent policy rule "${rule}" matched, so the message was not sent.${after}`;
        }
        return `${rule ? (builtInRuleText(rule) ?? "") : ""}${rule ? " " : ""}No consent allowed this message.${after}`.trim();
      }
      if (reason === "suppressed") {
        const text = message.deciding_rule ? builtInRuleText(message.deciding_rule) : null;
        return `${text ? `${text} ` : ""}A suppressed address is never sent to.${after}`.trim();
      }
      return `${reason ? reasonLabel(reason) : "The message was skipped"}.${after}`;
    }
    case "failed":
      return `${reason ? reasonLabel(reason) : "The message failed"}.${after}`;
    case "pending":
      if (message.deferred_until) {
        const why = waitLabel(message.deferred_why);
        return `${why ? `Held by the ${why}` : "Held"} until ${formatDateTime(message.deferred_until)}. Every check runs again then.`;
      }
      return "The message is queued and has not been judged yet.";
    default:
      return "The message is being handed to the destination.";
  }
}
