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
};

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
