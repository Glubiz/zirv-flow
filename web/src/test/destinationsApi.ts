/** Fixtures for the destination, forwarding and sending-domain tests. */

export const WS = "/api/v1/workspaces/ws1";

export const DEST_ID = "55555555-0000-4000-8000-000000000001";
export const OTHER_DEST_ID = "55555555-0000-4000-8000-000000000002";
export const DOMAIN_ID = "66666666-0000-4000-8000-000000000001";
export const IDENTITY_ID = "77777777-0000-4000-8000-000000000001";
export const CRED_ID = "88888888-0000-4000-8000-000000000001";
export const LETTER_ID = "99999999-0000-4000-8000-000000000001";
export const OTHER_LETTER_ID = "99999999-0000-4000-8000-000000000002";
export const RULE_ID = "aaaaaaaa-0000-4000-8000-000000000001";

export function destination(overrides: Record<string, unknown> = {}) {
  return {
    id: DEST_ID,
    name: "Order hook",
    kind: "webhook",
    archived: false,
    burst: 20,
    rate: 10,
    config: { url: "https://hooks.example.test/orders" },
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-02T09:00:00Z",
    credential_fields_set: ["signing_secret"],
    credential_id: CRED_ID,
    ...overrides,
  };
}

export function record(kind: "dkim" | "spf" | "dmarc", overrides: Record<string, unknown> = {}) {
  const base = {
    dkim: { host: "zirv._domainkey.mail.example.test", expected_text: "v=DKIM1; k=rsa; p=AAAA" },
    spf: { host: "mail.example.test", expected_text: "v=spf1 a:smtp.example.test ~all" },
    dmarc: { host: "_dmarc.mail.example.test", expected_text: "v=DMARC1; p=none" },
  }[kind];
  return { kind, ...base, status: "pending", observed_text: [], checked_at: null, ...overrides };
}

export function domain(overrides: Record<string, unknown> = {}) {
  return {
    id: DOMAIN_ID,
    domain: "mail.example.test",
    selector: "zirv",
    status: "unverified",
    created_at: "2026-10-01T09:00:00Z",
    last_checked_at: "2026-10-02T06:12:00Z",
    verified_at: null,
    records: [
      record("dkim", { status: "ok", observed_text: ["v=DKIM1; k=rsa; p=AAAA"] }),
      record("spf", { status: "mismatch", observed_text: ["v=spf1 include:_spf.relay.test -all"] }),
      record("dmarc", { status: "missing" }),
    ],
    ...overrides,
  };
}

export function identity(overrides: Record<string, unknown> = {}) {
  return {
    id: IDENTITY_ID,
    domain_id: DOMAIN_ID,
    archived: false,
    from_name: "Example",
    from_address: "hello@mail.example.test",
    reply_to: null,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-01T09:00:00Z",
    ...overrides,
  };
}

export function letter(overrides: Record<string, unknown> = {}) {
  return {
    id: LETTER_ID,
    attempts: 8,
    created_at: "2026-10-01T09:00:00Z",
    died_at: "2026-10-02T09:00:00Z",
    destination_id: DEST_ID,
    rule_id: RULE_ID,
    event_id: "bbbbbbbb-0000-4000-8000-000000000001",
    error: "http_4xx",
    ...overrides,
  };
}

export function rule(overrides: Record<string, unknown> = {}) {
  return {
    id: RULE_ID,
    name: "Paid orders",
    event_name: "purchase",
    destination_id: DEST_ID,
    active: true,
    filter: [],
    mapping: { id: { from: "event.id" } },
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-01T09:00:00Z",
    ...overrides,
  };
}

export function queue(overrides: Record<string, unknown> = {}) {
  return { destination_id: DEST_ID, depth: 4, oldest_lag_seconds: 125, dead: 2, ...overrides };
}

/** A workspace credential holding the named fields, as the credential picker reads them. */
export function credential(id: string, name: string, fields: string[]) {
  return {
    id,
    key: id,
    name,
    kind: "custom",
    schema_version: 1,
    schema: { fields: fields.map((key) => ({ key, label: key, type: "string", required: true, secret: true, options: [] })) },
    fields: {},
    hints: Object.fromEntries(fields.map((key) => [key, ""])),
    fingerprint: null,
    same_secret_as: [],
    used_by: [],
    rotated_at: "2026-10-01T09:00:00Z",
    last_tested_at: null,
    last_test_outcome: null,
    archived: false,
    created_by: null,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-01T09:00:00Z",
  };
}

/** The destination types the API lists, as the registry declares them. */
export function destinationKinds() {
  const field = (key: string, label: string, kind: string, required: boolean, secret: boolean) => ({
    key,
    label,
    help: "",
    kind,
    options: [],
    required,
    secret,
  });
  const capabilities = (channels: string[], rateLimit: boolean) => ({
    channels,
    idempotency_key: true,
    batching: false,
    rate_limit: rateLimit,
    erase: false,
  });
  return [
    {
      kind: "webhook",
      label: "Webhook",
      description: "A signed HTTP POST to a URL.",
      config_fields: [field("url", "URL", "url", true, false)],
      credential_fields: [field("signing_secret", "Signing secret", "text", true, true)],
      capabilities: capabilities(["webhook", "event"], false),
    },
    {
      kind: "smtp",
      label: "SMTP",
      description: "Email through a mail server or relay.",
      config_fields: [field("ca_certificate", "CA certificate", "long_text", false, false)],
      credential_fields: [field("host", "Host", "text", true, false), field("password", "Password", "text", true, true)],
      capabilities: capabilities(["email"], true),
    },
  ];
}

export function statsBody(overrides: Record<string, unknown> = {}) {
  return {
    window_hours: 24,
    attempts: 10,
    outcomes: { delivered: 6, retryable: 2, terminal: 1, throttled: 1 },
    status_classes: { none: 1, "2xx": 6, "3xx": 0, "4xx": 1, "5xx": 2 },
    ...overrides,
  };
}
