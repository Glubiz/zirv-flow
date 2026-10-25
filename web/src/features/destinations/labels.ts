import type { components } from "../../api";
import type { Tone } from "../../ui";

export type Destination = components["schemas"]["DestinationBody"];
export type CheckResult = components["schemas"]["DestinationCheckBody"];
export type QueueStatus = components["schemas"]["QueueStatusBody"];
export type DomainStatus = components["schemas"]["DomainStatusBody"];
export type Rule = components["schemas"]["RuleBody"];
export type DeadLetter = components["schemas"]["DeadLetterBody"];

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

/**
 * The destination types this build ships. The API has no listing of them yet (sources have
 * `/source-kinds`), so the fields mirror each type's spec and the server still validates them.
 */
export const DESTINATION_KINDS: readonly DestinationKind[] = [
  {
    kind: "webhook",
    label: "Webhook",
    description: "A signed HTTP POST to a URL.",
    config: [{ key: "url", label: "URL", help: "The address that receives the call.", type: "url", required: true }],
    credential: [
      { key: "signing_secret", label: "Signing secret", required: true, secret: true },
      { key: "previous_signing_secret", label: "Previous signing secret", required: false, secret: true },
    ],
    forwards: true,
    rateLimited: false,
  },
  {
    kind: "smtp",
    label: "SMTP",
    description: "Email through a mail server or relay, signed with the sending domain's DKIM key.",
    config: [
      {
        key: "ca_certificate",
        label: "CA certificate",
        help: "Trust only this certificate authority (PEM) for the server. Empty trusts the public roots.",
        type: "long-text",
        required: false,
      },
    ],
    credential: [
      { key: "host", label: "Host", required: true, secret: false },
      { key: "port", label: "Port", required: true, secret: false },
      { key: "tls", label: "TLS mode", required: true, secret: false },
      { key: "username", label: "Username", required: true, secret: false },
      { key: "password", label: "Password", required: true, secret: true },
    ],
    forwards: false,
    rateLimited: true,
  },
  {
    kind: "provider",
    label: "Email provider (manifest)",
    description: "An email provider's HTTP API, described by a manifest.",
    config: [
      {
        key: "manifest",
        label: "Manifest",
        help: "A JSON description of the provider. It holds no secret: the credential holds those.",
        type: "json",
        required: true,
      },
    ],
    credential: [
      { key: "api_key", label: "API key", required: false, secret: true },
      { key: "token", label: "Bearer token", required: false, secret: true },
      { key: "username", label: "User name", required: false, secret: true },
      { key: "password", label: "Password", required: false, secret: true },
    ],
    forwards: false,
    rateLimited: true,
  },
];

export function kindOf(kind: string): DestinationKind | null {
  return DESTINATION_KINDS.find((entry) => entry.kind === kind) ?? null;
}

export function kindLabel(kind: string): string {
  return kindOf(kind)?.label ?? kind;
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

export function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}
