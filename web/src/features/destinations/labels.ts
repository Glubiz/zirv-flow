import type { components } from "../../api";
import type { Tone } from "../../ui";

export type Destination = components["schemas"]["DestinationBody"];
export type CheckResult = components["schemas"]["DestinationCheckBody"];
export type QueueStatus = components["schemas"]["QueueStatusBody"];
export type DomainStatus = components["schemas"]["DomainStatusBody"];
export type Rule = components["schemas"]["RuleBody"];
export type DeadLetter = components["schemas"]["DeadLetterBody"];
export type DestinationStats = components["schemas"]["DestinationStatsBody"];

/** A field of a destination type's settings, as the API's field specs describe them. */
export interface ConfigField {
  key: string;
  label: string;
  help: string;
  type: "url" | "text" | "long-text" | "json";
  required: boolean;
}

/** A credential field a destination type reads, for the credential picker. */
export interface CredentialHint {
  key: string;
  label: string;
  required: boolean;
  secret: boolean;
}

/** One destination type, from the registry through the API. */
export interface DestinationKind {
  kind: string;
  label: string;
  description: string;
  config: ConfigField[];
  credential: CredentialHint[];
  /** Whether the type carries events, so a forwarding rule may name it. */
  forwards: boolean;
  /** Whether the type has a rate bucket of its own. */
  rateLimited: boolean;
}

type KindBody = components["schemas"]["DestinationKindBody"];
type FieldBody = KindBody["config_fields"][number];

const FIELD_TYPES: Record<string, ConfigField["type"]> = { url: "url", long_text: "long-text", json: "json" };

/** A destination type as the form uses it, from the spec the API lists in `/destination-kinds`. */
export function kindFromSpec(spec: KindBody): DestinationKind {
  const config = (field: FieldBody): ConfigField => ({
    key: field.key,
    label: field.label,
    help: field.help,
    type: FIELD_TYPES[field.kind] ?? "text",
    required: field.required,
  });
  return {
    kind: spec.kind,
    label: spec.label,
    description: spec.description,
    config: spec.config_fields.map(config),
    credential: spec.credential_fields.map((field) => ({
      key: field.key,
      label: field.label,
      required: field.required,
      secret: field.secret,
    })),
    forwards: spec.capabilities.channels.includes("event"),
    rateLimited: spec.capabilities.rate_limit,
  };
}

export function kindOf(kinds: readonly DestinationKind[], kind: string): DestinationKind | null {
  return kinds.find((entry) => entry.kind === kind) ?? null;
}

/** The type's own label; the raw id while the kinds load or for a type this build lacks. */
export function kindLabel(kinds: readonly DestinationKind[], kind: string): string {
  return kindOf(kinds, kind)?.label ?? kind;
}

/** What the person typed, by config field key. */
export type Values = Record<string, string>;

export function valuesFrom(kind: DestinationKind, config: Record<string, unknown>): Values {
  const values: Values = {};
  for (const field of kind.config) {
    const current = config[field.key];
    values[field.key] =
      field.type === "json"
        ? current === undefined
          ? ""
          : JSON.stringify(current, null, 2)
        : typeof current === "string"
          ? current
          : "";
  }
  return values;
}

export type ConfigResult = { ok: true; config: Record<string, unknown> } | { ok: false; errors: Record<string, string> };

/** The config to send: empty optional fields left out, JSON parsed. */
export function configFrom(kind: DestinationKind, values: Values): ConfigResult {
  const config: Record<string, unknown> = {};
  const errors: Record<string, string> = {};
  for (const field of kind.config) {
    const text = (values[field.key] ?? "").trim();
    if (text === "") {
      if (field.required) errors[field.key] = `${field.label} is required.`;
      continue;
    }
    if (field.type === "json") {
      try {
        const parsed: unknown = JSON.parse(text);
        if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
          errors[field.key] = `${field.label} must be a JSON object.`;
        } else {
          config[field.key] = parsed;
        }
      } catch {
        errors[field.key] = `${field.label} is not valid JSON.`;
      }
    } else {
      config[field.key] = text;
    }
  }
  return Object.keys(errors).length > 0 ? { ok: false, errors } : { ok: true, config };
}

const CHECK_CODES: Record<string, string> = {
  config_invalid: "The settings are not valid.",
  credential_refused: "The credential was refused.",
  unreachable: "The destination could not be reached.",
  egress_denied: "The address is not allowed by the outbound policy.",
  network_error: "A network error stopped the check.",
  upstream_error: "The destination answered with an error.",
  rate_limited: "The destination is limiting requests.",
  unsupported: "This type has no check.",
  failed: "The check failed.",
};

export function checkCodeText(code: string | null | undefined): string {
  return code ? (CHECK_CODES[code] ?? code) : "";
}

/** The lag of the oldest waiting row, as a short phrase. */
export function lagText(seconds: number): string {
  if (seconds <= 0) return "No lag";
  if (seconds < 60) return `${seconds} s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} h`;
  return `${Math.floor(seconds / 86400)} d`;
}

export type ProviderState = "verified" | "pending" | "failed";

export function providerStatusText(status: string): { text: string; tone: Tone } {
  switch (status) {
    case "verified":
      return { text: "Verified at the provider", tone: "neutral" };
    case "pending":
      return { text: "Pending at the provider", tone: "attention" };
    case "failed":
      return { text: "Failed at the provider", tone: "error" };
    default:
      return { text: "Unknown at the provider", tone: "attention" };
  }
}

