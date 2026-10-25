import type { components } from "../../api";

export type Cadence = components["schemas"]["SegmentBody"]["cadence_minutes"];

/** The thirteen cadences the API accepts, in minutes, with how each reads. */
export const CADENCES: readonly { minutes: Cadence; label: string }[] = [
  { minutes: 5, label: "every 5 minutes" },
  { minutes: 10, label: "every 10 minutes" },
  { minutes: 15, label: "every 15 minutes" },
  { minutes: 30, label: "every 30 minutes" },
  { minutes: 60, label: "every hour" },
  { minutes: 120, label: "every 2 hours" },
  { minutes: 180, label: "every 3 hours" },
  { minutes: 240, label: "every 4 hours" },
  { minutes: 360, label: "every 6 hours" },
  { minutes: 480, label: "every 8 hours" },
  { minutes: 720, label: "every 12 hours" },
  { minutes: 1440, label: "every day" },
  { minutes: 10080, label: "every week" },
];

export function cadenceLabel(minutes: number): string {
  return CADENCES.find((cadence) => cadence.minutes === minutes)?.label ?? `every ${minutes} minutes`;
}

export function asCadence(value: string): Cadence | null {
  return CADENCES.find((cadence) => String(cadence.minutes) === value)?.minutes ?? null;
}

const ISSUES: Record<string, string> = {
  unsupported_version: "This definition version is not supported.",
  malformed: "This part of the definition is not shaped correctly.",
  unknown_key: "This part of the definition has a key that is not recognised.",
  unknown_trait: "This field does not exist in the workspace.",
  unknown_event: "This event is not known to the workspace.",
  unknown_property: "This property is not part of the event.",
  unknown_list: "This list does not exist.",
  unknown_segment: "This segment does not exist.",
  unknown_topic: "This topic does not exist for the channel.",
  invalid_identifier_kind: "This is not an identifier kind.",
  operator_not_allowed: "This comparison does not apply to the type of value.",
  value_type_mismatch: "The value does not fit the type.",
  value_missing: "A value is needed.",
  value_not_allowed: "This comparison takes no value, or this value is not allowed.",
  aggregate_not_allowed: "This summary does not apply here.",
  invalid_duration: "Use a duration in days or hours, such as P7D or PT12H.",
  window_too_long: "The duration is longer than three years.",
  invalid_range: "The start must come before the end.",
  too_deep: "Groups are nested too deeply.",
  too_many_nodes: "The definition has too many conditions.",
  too_many_values: "Too many values in the list.",
  value_too_long: "The value is too long.",
  unknown_dimension: "This dimension does not exist.",
  policy_only: "This condition is only for a consent policy.",
};

export function issueMessage(code: string): string {
  return ISSUES[code] ?? `The definition is not valid (${code}).`;
}

export function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

const CAUSES: Record<string, string> = {
  initial: "first computation",
  definition_change: "definition change",
  time: "time passing",
  manual: "manual refresh",
  data_change: "data change",
};

export function causeLabel(cause: string): string {
  return CAUSES[cause] ?? cause;
}
