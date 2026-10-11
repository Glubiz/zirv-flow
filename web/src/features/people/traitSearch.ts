/** How a trait is compared in a people search, by the trait's registry type. */

export interface Operator {
  value: string;
  label: string;
}

const IS: Operator = { value: "eq", label: "is" };
const EXISTS: Operator = { value: "exists", label: "has a value" };
const ORDERED: Operator[] = [
  IS,
  { value: "gt", label: "is after or above" },
  { value: "gte", label: "is at least" },
  { value: "lt", label: "is before or below" },
  { value: "lte", label: "is at most" },
  EXISTS,
];

const OPERATORS: Record<string, Operator[]> = {
  string: [IS, { value: "prefix", label: "starts with" }, EXISTS],
  number: ORDERED,
  timestamp: ORDERED,
  date: ORDERED,
  boolean: [IS, EXISTS],
  string_list: [{ value: "eq", label: "contains" }, EXISTS],
};

export function operatorsFor(type: string): Operator[] {
  return OPERATORS[type] ?? [IS, EXISTS];
}

export type ParsedValue = { ok: true; value: unknown } | { ok: false; message: string };

/** The JSON value to search for, from what was typed, or why it cannot be one. */
export function parseSearchValue(type: string, operator: string, text: string): ParsedValue {
  if (operator === "exists") {
    return { ok: true, value: undefined };
  }
  const trimmed = text.trim();
  if (trimmed === "") {
    return { ok: false, message: "Enter a value to search for." };
  }
  switch (type) {
    case "number": {
      const number = Number(trimmed);
      return Number.isFinite(number) ? { ok: true, value: number } : { ok: false, message: "Enter a number." };
    }
    case "boolean":
      return trimmed === "true" || trimmed === "false"
        ? { ok: true, value: trimmed === "true" }
        : { ok: false, message: "Choose yes or no." };
    case "date":
      return /^\d{4}-\d{2}-\d{2}$/.test(trimmed)
        ? { ok: true, value: trimmed }
        : { ok: false, message: "Enter a date as YYYY-MM-DD." };
    case "timestamp":
      return Number.isNaN(Date.parse(trimmed))
        ? { ok: false, message: "Enter a date and time such as 2026-01-31T10:00:00Z." }
        : { ok: true, value: trimmed };
    default:
      return { ok: true, value: text };
  }
}

export function valueHint(type: string): string | undefined {
  switch (type) {
    case "date":
      return "A date as YYYY-MM-DD.";
    case "timestamp":
      return "A date and time such as 2026-01-31T10:00:00Z.";
    case "number":
      return "A number.";
    case "string_list":
      return "One item; matches a person whose list holds it.";
    default:
      return undefined;
  }
}
