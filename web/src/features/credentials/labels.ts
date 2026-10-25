import type { components } from "../../api";

export type Credential = components["schemas"]["CredentialBody"];
export type CredentialKind = components["schemas"]["KindBody"];
export type Schema = components["schemas"]["SchemaBody"];
export type SchemaField = components["schemas"]["SchemaFieldBody"];

/** What the person typed, by field key. */
export type Values = Record<string, string>;

/** The slice of an implementation's own credential field spec the inline form needs. */
export interface FieldHint {
  key: string;
  label: string;
  required: boolean;
  secret: boolean;
  kind?: string;
  options?: string[];
}

export function isSecret(field: SchemaField): boolean {
  return field.secret === true;
}

export function isRequired(field: SchemaField): boolean {
  return field.required === true;
}

/** The fields of a schema in one group: the secret ones or the rest. */
export function fieldsOf(schema: Schema, group: "secret" | "public"): SchemaField[] {
  return schema.fields.filter((field) => isSecret(field) === (group === "secret"));
}

/** Empty text for every field of one group, or the stored non-secret values as text. */
export function emptyValues(schema: Schema, group: "secret" | "public", stored: Record<string, unknown> = {}): Values {
  const values: Values = {};
  for (const field of fieldsOf(schema, group)) {
    const current = group === "public" ? stored[field.key] : undefined;
    values[field.key] = typeof current === "string" ? current : typeof current === "number" || typeof current === "boolean" ? String(current) : "";
  }
  return values;
}

/** Whether every required field of the group has a value. */
export function complete(schema: Schema, group: "secret" | "public", values: Values): boolean {
  return fieldsOf(schema, group).every((field) => !isRequired(field) || (values[field.key] ?? "").trim() !== "");
}

/** The non-secret values to send: empty optional fields are left out, integers typed. */
export function publicFrom(schema: Schema, values: Values): Record<string, unknown> {
  const fields: Record<string, unknown> = {};
  for (const field of fieldsOf(schema, "public")) {
    const text = (values[field.key] ?? "").trim();
    if (text === "") continue;
    fields[field.key] = field.type === "integer" ? Number(text) : text;
  }
  return fields;
}

/** The secret values to send, exactly as typed. */
export function secretFrom(schema: Schema, values: Values): Record<string, string> {
  const secret: Record<string, string> = {};
  for (const field of fieldsOf(schema, "secret")) {
    const text = values[field.key] ?? "";
    if (text !== "") secret[field.key] = text;
  }
  return secret;
}

/** A schema for an inline credential, from the credential fields an implementation declares. */
export function schemaFromHints(hints: FieldHint[]): Schema {
  return {
    fields: hints.map((hint) => ({
      key: hint.key,
      label: hint.label,
      type: hint.kind === "select" ? "enum" : hint.kind === "number" ? "integer" : hint.kind === "url" ? "url" : "string",
      required: hint.required,
      secret: hint.secret,
      options: hint.kind === "select" ? (hint.options ?? []) : [],
    })),
  };
}

/** The field names a credential holds: its non-secret fields and its secret fields. */
export function fieldNames(credential: Credential): string[] {
  return [...new Set([...Object.keys(credential.fields), ...Object.keys(credential.hints)])];
}

/**
 * Whether the credential fits an owner that declares `fields`: it holds every required field and
 * at least one declared field. An owner that declares no field fits no credential.
 */
export function covers(credential: Credential, fields: Pick<FieldHint, "key" | "required">[]): boolean {
  const names = fieldNames(credential);
  return (
    fields.filter((field) => field.required).every((field) => names.includes(field.key)) &&
    fields.some((field) => names.includes(field.key))
  );
}

/** A secret as the list shows it: masked, with its last four characters when the server sent them. */
export function maskedHint(hint: string | undefined): string {
  return hint ? `••••${hint}` : "••••••••";
}

const BUILT_IN_KINDS: Record<string, string> = {
  api_key: "API key",
  basic: "Username and password",
  smtp: "SMTP server",
  oauth2_client: "OAuth2 client",
  signing_secret: "Signing secret",
  bearer_token: "Bearer token",
  tls_client: "TLS client certificate",
};

/** The label of a kind: the server's list when given, else the built-in names. */
export function kindName(kinds: CredentialKind[] | null, kind: string): string {
  if (kind === "custom") return "Custom";
  if (kind === "imported") return "Imported";
  return kinds?.find((entry) => entry.id === kind)?.label ?? BUILT_IN_KINDS[kind] ?? kind;
}

export function testOutcomeLabel(outcome: string | null | undefined): string {
  return outcome === "ok" ? "Passed" : outcome === "failed" ? "Failed" : "Not tested";
}
