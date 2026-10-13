import type { components } from "../../api";

export type Source = components["schemas"]["SourceBody"];
export type SourceKind = components["schemas"]["SourceKindBody"];
export type FieldSpec = components["schemas"]["FieldSpecBody"];
export type Stream = components["schemas"]["StreamBody"];

export function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

export function syncModeLabel(mode: string): string {
  return mode === "incremental" ? "Incremental" : mode === "full_refresh" ? "Full refresh" : mode;
}

/** The label of a source type, or the type's own id when this build does not list it. */
export function kindName(kinds: SourceKind[] | null, kind: string): string {
  return kinds?.find((entry) => entry.kind === kind)?.label ?? kind;
}

/** What the person typed into the form, by field key. A checkbox is "true" or "false". */
export type Values = Record<string, string>;

/** A setting as text for a form or a fact list; anything but text, numbers and booleans is JSON. */
export function showValue(value: unknown): string {
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return value === undefined || value === null ? "" : JSON.stringify(value);
}

export function initialValues(fields: FieldSpec[], config: Record<string, unknown> = {}): Values {
  const values: Values = {};
  for (const field of fields) {
    const current = config[field.key];
    values[field.key] = field.secret ? "" : showValue(current);
  }
  return values;
}

/** The values with every secret field emptied, so a credential does not outlive the request that sent it. */
export function withoutSecrets(fields: FieldSpec[], values: Values): Values {
  const kept = { ...values };
  for (const field of fields) {
    if (field.secret) kept[field.key] = "";
  }
  return kept;
}

/** The settings object to send: empty optional fields are left out, numbers and booleans typed. */
export function configFrom(fields: FieldSpec[], values: Values): Record<string, unknown> {
  const config: Record<string, unknown> = {};
  for (const field of fields) {
    const text = (values[field.key] ?? "").trim();
    if (text === "") continue;
    config[field.key] = field.kind === "number" ? Number(text) : field.kind === "bool" ? text === "true" : text;
  }
  return config;
}

/** The credential fields that were filled in, by name. */
export function credentialFrom(fields: FieldSpec[], values: Values): Record<string, string> {
  const credential: Record<string, string> = {};
  for (const field of fields) {
    const text = values[field.key] ?? "";
    if (text !== "") credential[field.key] = text;
  }
  return credential;
}

/** Whether every required field has a value. */
export function filled(fields: FieldSpec[], values: Values): boolean {
  return fields.every((field) => !field.required || (values[field.key] ?? "").trim() !== "" || field.kind === "bool");
}
