/** The closed list of sources a forwarding mapping may read (README, forwarding). */
export const SOURCES = [
  { id: "event.id", label: "Event id" },
  { id: "event.name", label: "Event name" },
  { id: "event.time", label: "Event time" },
  { id: "event.received_at", label: "Time received" },
  { id: "person.id", label: "Person id" },
  { id: "event.properties", label: "Event property" },
  { id: "value", label: "Fixed value" },
] as const;

export type SourceId = (typeof SOURCES)[number]["id"];
export type ValueType = "string" | "number" | "boolean";

export interface MappingRow {
  key: string;
  source: SourceId;
  /** A JSON pointer into the event properties, such as `/order/total`. */
  pointer: string;
  valueType: ValueType;
  /** The fixed value as typed. */
  value: string;
  /** An entry this editor does not understand, kept as it came and sent back unchanged. */
  raw?: { entry: unknown };
}

export function newRow(): MappingRow {
  return { key: "", source: "event.id", pointer: "", valueType: "string", value: "" };
}

const PLAIN: readonly string[] = ["event.id", "event.name", "event.time", "event.received_at", "person.id"];

export function rowsFromMapping(mapping: Record<string, unknown>): MappingRow[] {
  return Object.entries(mapping).map(([key, source]) => {
    const row = { ...newRow(), key };
    if (typeof source === "object" && source !== null && !Array.isArray(source)) {
      const from = (source as { from?: unknown }).from;
      if (typeof from === "string" && PLAIN.includes(from)) {
        return { ...row, source: from as SourceId };
      }
      if (from === "event.properties") {
        const pointer = (source as { pointer?: unknown }).pointer;
        if (typeof pointer === "string") return { ...row, source: "event.properties" as const, pointer };
      } else if ("value" in source) {
        const value = source.value;
        if (typeof value === "number") return { ...row, source: "value" as const, valueType: "number" as const, value: String(value) };
        if (typeof value === "boolean") return { ...row, source: "value" as const, valueType: "boolean" as const, value: String(value) };
        if (typeof value === "string") return { ...row, source: "value" as const, value };
      }
    }
    return { ...row, raw: { entry: source } };
  });
}

export type MappingResult =
  | { ok: true; mapping: Record<string, unknown> }
  | { ok: false; errors: Record<number, string> };

/** The mapping to send, or what is wrong with each row (by position). */
export function mappingFromRows(rows: readonly MappingRow[]): MappingResult {
  const mapping: Record<string, unknown> = {};
  const errors: Record<number, string> = {};
  const seen = new Set<string>();
  rows.forEach((row, at) => {
    const key = row.key.trim();
    if (key === "") {
      errors[at] = "Give the output key a name.";
      return;
    }
    if (seen.has(key)) {
      errors[at] = `The output key ${key} is used twice.`;
      return;
    }
    seen.add(key);
    if (row.raw) {
      mapping[key] = row.raw.entry;
    } else if (row.source === "event.properties") {
      if (!row.pointer.startsWith("/")) {
        errors[at] = "A property pointer starts with a slash, such as /order/total.";
        return;
      }
      mapping[key] = { from: "event.properties", pointer: row.pointer };
    } else if (row.source === "value") {
      if (row.valueType === "number") {
        const number = Number(row.value);
        if (row.value.trim() === "" || !Number.isFinite(number)) {
          errors[at] = "A fixed number needs a number.";
          return;
        }
        mapping[key] = { value: number };
      } else if (row.valueType === "boolean") {
        mapping[key] = { value: row.value === "true" };
      } else {
        mapping[key] = { value: row.value };
      }
    } else {
      mapping[key] = { from: row.source };
    }
  });
  return Object.keys(errors).length > 0 ? { ok: false, errors } : { ok: true, mapping };
}
