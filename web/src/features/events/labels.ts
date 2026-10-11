import type { components } from "../../api";

export type EventSchema = components["schemas"]["EventSchemaBody"];
export type EventSchemaDetail = components["schemas"]["EventSchemaDetailBody"];
export type SchemaProperty = components["schemas"]["PropertyBody"];

export const PROPERTY_TYPES: { value: string; label: string }[] = [
  { value: "string", label: "Text" },
  { value: "number", label: "Number" },
  { value: "integer", label: "Whole number" },
  { value: "boolean", label: "Yes or no" },
  { value: "timestamp", label: "Date and time" },
  { value: "date", label: "Date" },
  { value: "string_list", label: "List of text" },
  { value: "object", label: "Object" },
];

export const MODES: { value: string; label: string; help: string }[] = [
  { value: "allow", label: "Allow", help: "Events that break the schema are accepted and are not flagged" },
  { value: "warn", label: "Warn", help: "Events that break the schema are accepted and flagged" },
  { value: "block", label: "Block", help: "Events that break the schema are rejected and never stored" },
];

export const UNPLANNED: { value: string; label: string; help: string }[] = [
  { value: "draft", label: "Accept and draft a schema", help: "The event is stored and a draft schema is inferred from its shape. Nothing is published." },
  { value: "reject", label: "Reject", help: "The event is refused and listed under rejects." },
];

export const REASONS: Record<string, string> = {
  missing_required: "A required property is missing",
  wrong_type: "A property has the wrong type",
  not_in_enum: "A property is not one of the allowed values",
  unplanned_property: "A property is not in the schema",
};

export const REJECT_REASONS: Record<string, string> = {
  validation_failed: "Failed validation",
  unplanned_event: "No published schema",
  no_identifier: "No identifier",
  invalid_event_name: "Invalid event name",
  invalid_message_id: "Invalid message id",
  apply_failed: "Could not be applied",
  no_person: "Names no person",
  not_replayable: "Cannot be replayed",
};

export const OUTCOMES: { value: string; label: string }[] = [
  { value: "accepted", label: "Accepted" },
  { value: "flagged", label: "Flagged" },
  { value: "unplanned", label: "Unplanned" },
  { value: "rejected", label: "Rejected" },
  { value: "duplicate", label: "Duplicate" },
];

export const FILTER_OPS: { value: string; label: string }[] = [
  { value: "eq", label: "is" },
  { value: "ne", label: "is not" },
  { value: "in", label: "is one of" },
  { value: "prefix", label: "starts with" },
  { value: "gt", label: "is greater than" },
  { value: "gte", label: "is at least" },
  { value: "lt", label: "is less than" },
  { value: "lte", label: "is at most" },
  { value: "exists", label: "is present" },
];

/** The operators a property type takes; an undeclared property compares as text. */
export function operatorsFor(type: string | null): string[] {
  switch (type) {
    case "string":
      return ["eq", "ne", "in", "prefix", "exists"];
    case "number":
    case "integer":
      return ["eq", "ne", "in", "gt", "gte", "lt", "lte", "exists"];
    case "boolean":
    case "string_list":
      return ["eq", "ne", "exists"];
    case "timestamp":
    case "date":
      return ["eq", "ne", "gt", "gte", "lt", "lte", "exists"];
    case "object":
      return ["exists"];
    default:
      return ["eq", "ne", "in", "prefix", "gt", "gte", "lt", "lte", "exists"];
  }
}

export const REPLAY_OUTCOMES: Record<string, string> = {
  accept: "Will be stored",
  flag: "Will be stored, flagged",
  still_rejected: "Still rejected",
  duplicate: "Already stored",
  already_replayed: "Already replayed",
};

export function outcomeLabel(value: string): string {
  return OUTCOMES.find((outcome) => outcome.value === value)?.label ?? value;
}

export function opLabel(value: string): string {
  return FILTER_OPS.find((op) => op.value === value)?.label ?? value;
}

export function typeLabel(value: string): string {
  return PROPERTY_TYPES.find((type) => type.value === value)?.label ?? value;
}

export function modeLabel(value: string): string {
  return MODES.find((mode) => mode.value === value)?.label ?? value;
}

export function reasonLabel(code: string): string {
  return REASONS[code] ?? code;
}

export function rejectReasonLabel(code: string): string {
  return REJECT_REASONS[code] ?? code;
}

/** Splits a comma-separated list, dropping blanks. */
export function splitList(text: string): string[] {
  return text
    .split(",")
    .map((item) => item.trim())
    .filter((item) => item !== "");
}
