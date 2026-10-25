import type { components } from "../../api";
import { formatAgo, formatDuration } from "../../ui";

import { analyze } from "./manifest";
import { parseQueries, queriesValid } from "./queries";

export type Source = components["schemas"]["SourceBody"];
export type SourceKind = components["schemas"]["SourceKindBody"];
export type FieldSpec = components["schemas"]["FieldSpecBody"];
export type Stream = components["schemas"]["StreamBody"];

export function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

export function syncModeLabel(mode: string): string {
  const labels: Record<string, string> = { incremental: "Incremental", full_refresh: "Full refresh", stream: "Streaming" };
  return labels[mode] ?? mode;
}

/** The label of a source type, or the type's own id when this build does not list it. */
export function kindName(kinds: SourceKind[] | null, kind: string): string {
  return kinds?.find((entry) => entry.kind === kind)?.label ?? kind;
}

/** Whether the type's settings hold a manifest, which may declare credential fields of its own. */
export function hasManifest(kind: Pick<SourceKind, "config_fields">): boolean {
  return kind.config_fields.some((field) => field.key === "manifest");
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
    // A JSON setting is edited as indented text.
    values[field.key] = field.secret ? "" : field.kind === "json" && current !== undefined ? JSON.stringify(current, null, 2) : showValue(current);
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

/** The text as a JSON object, or null when it is not one. */
export function parseJsonObject(text: string): Record<string, unknown> | null {
  const analysis = analyze(text);
  if (!analysis.ok) return null;
  const value = analysis.value;
  return typeof value === "object" && value !== null && !Array.isArray(value) ? (value as Record<string, unknown>) : null;
}

/** The settings object to send: empty optional fields are left out, numbers and booleans typed. */
export function configFrom(fields: FieldSpec[], values: Values): Record<string, unknown> {
  const config: Record<string, unknown> = {};
  for (const field of fields) {
    const text = (values[field.key] ?? "").trim();
    if (text === "") continue;
    if (field.kind === "json") {
      // The form keeps the object as text; text that is not an object is left out and caught by
      // shapesValid before anything is sent.
      const parsed = parseJsonObject(text);
      if (parsed !== null) config[field.key] = parsed;
      continue;
    }
    if (field.kind === "queries") {
      // The form keeps the list as JSON text; an empty list is a setting that is not there.
      const rows = parseQueries(text);
      if (rows !== null && rows.length > 0) config[field.key] = rows;
      continue;
    }
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

export type Run = components["schemas"]["RunBody"];
export type RunDetail = components["schemas"]["RunDetailBody"];
export type RunReject = components["schemas"]["RunRejectBody"];

export function isActiveRun(run: Pick<Run, "state">): boolean {
  return run.state === "queued" || run.state === "running";
}

export function runStateLabel(state: string): string {
  const labels: Record<string, string> = {
    queued: "Queued",
    running: "Running",
    succeeded: "Succeeded",
    failed: "Failed",
    cancelled: "Cancelled",
  };
  return labels[state] ?? state;
}

export function runStateTone(state: string): "neutral" | "attention" | "error" {
  return state === "failed" ? "error" : state === "succeeded" ? "neutral" : "attention";
}

export function runKindLabel(kind: string): string {
  return kind === "reconcile" ? "Reconciliation" : kind === "sync" ? "Sync" : kind;
}

export function triggerLabel(trigger: string): string {
  return trigger === "schedule" ? "Schedule" : trigger === "manual" ? "Manual" : trigger;
}

/** What a failed run's error code means, in plain words. */
export function runErrorText(code: string, threshold: number): string {
  const percent = Math.round(threshold * 1000) / 10;
  const texts: Record<string, string> = {
    reject_threshold: `More than ${percent}% of the records were refused, so the run stopped. Records applied before that were kept, and nothing was removed.`,
    stream_not_mapped: "The stream has no mapping. Set one up, then run it again.",
    mapping_invalid: "The stream's mapping could not be read. Save the mapping again.",
    stream_settings_invalid:
      "The stream's settings no longer fit what the source offers. Discover the streams again, set the stream up, then run it again.",
    source_error: "The source could not be read after several tries. Test the connection, then run it again.",
    job_failed: "The run could not finish after several tries. Run it again; if it keeps failing, look at the jobs page.",
    file_changed: "The file changed while it was being read, so the run stopped. Records read before that were kept. Run it again.",
    file_range_unsupported:
      "The file is over 10 MiB and its server does not answer byte ranges. Ask the host to support them, or split the file.",
    file_range_invalid:
      "The file's server answered a byte range that does not add up, so the run stopped. Records read before that were kept. Run it again.",
    record_too_large: "One record is over 1 MiB, so the file could not be read.",
    csv_invalid: "The file is not valid CSV text.",
    json_invalid: "The file is not valid JSON.",
    list_missing: "A list the mapping adds people to has been deleted. Map the stream again, then run it again.",
    stream_not_keyed: "The mapping has an event, but the stream has no primary key. Set one up, then run it again.",
    event_key_unavailable: "The server has no key to protect event properties with, so events cannot be kept.",
    source_missing: "The source is gone.",
    stream_missing: "The stream is gone. Discover the streams again.",
    sweep_limit:
      "This run would have removed more records than the stream allows, so it removed none. Raise the limit on the stream to allow it.",
    source_kind_unavailable: "This build of the server does not include this type of source.",
    source_config_invalid:
      "The source's settings no longer pass the checks, so nothing was read. Open the source, fix the manifest, then run it again.",
    kafka_offset_out_of_range:
      "The topic no longer has the position this stream was at, so it may have been deleted and created again. Nothing was read. Stored positions cannot be reset from the app yet, so this needs an operator.",
    kafka_auth_failed: "The brokers refused the user name and password. Check the credential, then run the stream again.",
    kafka_tls_failed:
      "The secure connection to the brokers failed. Check the certificate authority and the client certificate, then run the stream again.",
    kafka_topic_missing: "A topic of this stream does not exist on the brokers. Check the topic names, then run the stream again.",
    kafka_connect_failed:
      "The brokers could not be reached after several tries. Test the connection, then run the stream again.",
  };
  return texts[code] ?? `The run failed (${code}).`;
}

/**
 * Whether the values that have a shape of their own hold something the server will accept the
 * shape of: for now, the queries of a database source. Blocks saving a form edited into nonsense.
 */
export function shapesValid(fields: FieldSpec[], values: Values): boolean {
  return fields.every(
    (field) =>
      (field.kind !== "queries" || queriesValid(values[field.key] ?? "")) &&
      (field.kind !== "json" || parseJsonObject(values[field.key] ?? "") !== null),
  );
}

export type Freshness = components["schemas"]["FreshnessBody"];

/** How far behind a streaming stream is: "Lag 1,204" once its consumer has looked at the log, else null. */
export function lagLabel(freshness: Pick<Freshness, "lag"> | null | undefined): string | null {
  const lag = freshness?.lag;
  return lag ? `Lag ${lag.total.toLocaleString("en-US")}` : null;
}

/** The lag of each partition, for a tooltip. */
export function lagTitle(freshness: Pick<Freshness, "lag"> | null | undefined): string | undefined {
  const lag = freshness?.lag;
  if (!lag) return undefined;
  return lag.partitions.map((entry) => `Partition ${entry.partition}: ${entry.lag.toLocaleString("en-US")}`).join("\n");
}

/** Why a message was refused, in words; the place (partition and offset) is shown beside it. */
export function rejectReasonText(code: string): string {
  const texts: Record<string, string> = {
    kafka_key_missing: "The message has no key, or an empty one.",
    kafka_key_not_text: "The message key is not text of up to 512 characters.",
    kafka_value_not_json: "The message is not JSON.",
    kafka_value_not_object: "The message is JSON but not an object.",
    kafka_message_too_large: "The message is over 1 MiB.",
  };
  return texts[code] ?? code;
}

/** The state of a stream or a source in words. Anything the server adds later shows as it is. */
export function freshnessLabel(state: string): string {
  const labels: Record<string, string> = { fresh: "Fresh", stale: "Stale", failing: "Failing" };
  return labels[state] ?? state;
}

export function freshnessTone(state: string): "neutral" | "attention" | "error" {
  return state === "failing" ? "error" : state === "stale" ? "attention" : "neutral";
}

/** When a stream last succeeded: "Never", or the time ago. */
export function lastSuccessLabel(freshness: Pick<Freshness, "last_success_at"> | null | undefined, now?: number): string {
  return freshness?.last_success_at ? formatAgo(freshness.last_success_at, now) : "Never";
}

/** The threshold in force: "None" when the stream never turns stale, else its length, marked when it is the default. */
export function thresholdLabel(freshness: Pick<Freshness, "threshold_seconds" | "threshold_is_default"> | null | undefined): string {
  if (!freshness) return "None";
  if (freshness.threshold_seconds === null || freshness.threshold_seconds === undefined) return "None";
  const text = formatDuration(freshness.threshold_seconds);
  return freshness.threshold_is_default ? `${text} (default)` : text;
}

/** Every code the manifest validator can return, as the server documents them, plus the one the editor adds. */
export const MANIFEST_ISSUE_CODES = [
  "syntax_error",
  "required",
  "invalid_type",
  "invalid_value",
  "out_of_range",
  "invalid_length",
  "invalid_url",
  "unknown_field",
  "expression_not_supported",
  "unsupported_version",
  "manifest_too_large",
  "too_many_streams",
  "too_many_fields",
  "too_deep",
  "too_many_headers",
  "forbidden_header",
  "invalid_header",
  "too_many_query",
  "path_invalid",
  "invalid_pointer",
  "duplicate_stream",
  "duplicate_field",
  "invalid_field_name",
  "primary_key_empty",
  "primary_key_unknown_field",
  "cursor_field_unknown",
  "invalid_mapping",
  "list_target_not_allowed",
  "no_identifier",
  "too_many_rules",
  "unknown_column",
  "invalid_identifier_kind",
  "invalid_trait_key",
  "unknown_trait",
  "archived_trait",
  "duplicate_trait",
  "invalid_transform",
  "invalid_event_name",
  "invalid_event_property",
  "duplicate_event_property",
  "too_many_event_properties",
] as const;

const ISSUE_TEXT: Record<(typeof MANIFEST_ISSUE_CODES)[number], string> = {
  syntax_error: "This is not valid JSON. Check the commas, quotes and brackets near here.",
  required: "A required setting is missing here.",
  invalid_type: "This has the wrong kind of value, for example text where a number or a list is expected.",
  invalid_value: "This is not one of the values allowed here.",
  out_of_range: "This number or list size is outside what is allowed.",
  invalid_length: "This text is empty or too long.",
  invalid_url: "This is not an allowed address. It must be a full https address without a user name or fragment.",
  unknown_field: "This setting does not exist in a manifest. Check the spelling.",
  expression_not_supported:
    "Expressions and scripts are not supported. Use a plain value, and the structure the manifest offers.",
  unsupported_version: 'Only manifest version "v1" is supported.',
  manifest_too_large: "The manifest is larger than 64 KiB.",
  too_many_streams: "A manifest can describe at most 20 streams.",
  too_many_fields: "A stream can declare at most 200 fields.",
  too_deep: "The manifest is nested too deeply (at most 12 levels).",
  too_many_headers: "At most 10 fixed headers are allowed.",
  forbidden_header: "This header is set by zirv-flow itself and cannot be fixed in a manifest.",
  invalid_header: "This header name or value is not allowed.",
  too_many_query: "A stream can have at most 20 fixed query parameters.",
  path_invalid: "A stream path must start with / and have no //, .., ? or # in it.",
  invalid_pointer: 'This must be a JSON pointer such as /data/items, or "" for a top-level list.',
  duplicate_stream: "Two streams have the same name.",
  duplicate_field: "Two fields of this stream have the same name.",
  invalid_field_name: "A field name cannot be blank.",
  primary_key_empty: "A stream needs at least one primary key field.",
  primary_key_unknown_field: "The primary key names a field the stream does not declare.",
  cursor_field_unknown: "The cursor field is not one of the stream's declared fields.",
  invalid_mapping: "The mapping is not in the shape the mapping editor saves.",
  list_target_not_allowed: "A manifest cannot add people to a list. Set that up in the mapping of the stream.",
  no_identifier: "The mapping needs at least one column that becomes an identifier.",
  too_many_rules: "The mapping has too many rules.",
  unknown_column: "The mapping uses a column that is not one of the stream's declared fields.",
  invalid_identifier_kind: "The mapping gives an identifier a kind that does not exist.",
  invalid_trait_key: "The mapping uses a trait key that is not a valid key.",
  unknown_trait: "The mapping uses a trait the workspace does not have.",
  archived_trait: "The mapping uses a trait that has been archived.",
  duplicate_trait: "Two columns of the mapping fill the same trait.",
  invalid_transform: "A step in the mapping is not valid.",
  invalid_event_name: "The event name in the mapping is not valid.",
  invalid_event_property: "An event property in the mapping is not valid.",
  duplicate_event_property: "Two event properties in the mapping have the same name.",
  too_many_event_properties: "The mapping's event has too many properties.",
};

/** What a validation code means, in a sentence. A code this build does not know is said plainly. */
export function issueText(code: string): string {
  return (ISSUE_TEXT as Record<string, string>)[code] ?? "This part of the manifest is not valid.";
}

/** What the page shows after the account connection came back to it. */
export function oauthReasonText(reason: string | null): string {
  const texts: Record<string, string> = {
    unknown_state: "The sign-in link had expired or was already used. Start again with Connect account.",
    state_mismatch:
      "The sign-in was finished by a different person or session than the one that started it, or the source's connection settings changed in between. Start again while signed in as yourself.",
    provider_error: "The provider did not allow the connection. Start again and approve the request.",
    exchange_failed: "The provider refused to hand over the account tokens. Check the client ID and secret, then try again.",
    no_refresh_token:
      "The provider did not give a refresh token, so syncs could not run unattended. Allow offline access in the provider's settings and the manifest's scopes, then try again.",
    source_gone: "The source changed or was deleted while you were signing in. Open it and try again.",
  };
  return texts[reason ?? ""] ?? "Connecting the account did not work. Try again.";
}

/** A JSON setting in a fact list: a manifest by its stream count, anything else as set. */
export function describeJson(value: unknown): string {
  const streams = (value as { streams?: unknown } | null)?.streams;
  return Array.isArray(streams) ? plural(streams.length, "stream", "streams") + " described" : "Set";
}

export type Webhook = components["schemas"]["WebhookBody"];
export type CreatedWebhook = components["schemas"]["CreatedWebhookBody"];

/** Whether records of this source type arrive by push (an inbound webhook) instead of being read. */
export function isPush(kind: Pick<SourceKind, "capabilities"> | null | undefined): boolean {
  return kind?.capabilities.push === true;
}

/** The address senders post to: the page's own origin and the hook's path. */
export function webhookUrl(origin: string, hook: Pick<Webhook, "path">): string {
  return `${origin}${hook.path}`;
}

/** The hook's signing scheme in a sentence. */
export function schemeSummary(hook: Pick<Webhook, "signature_header" | "signature_prefix" | "timestamp_header" | "tolerance_seconds">): string {
  const prefix = hook.signature_prefix === "" ? "no prefix" : `prefix ${hook.signature_prefix}`;
  if (hook.timestamp_header === null) {
    return `HMAC-SHA256 of the body in ${hook.signature_header} (${prefix}); no timestamp, so only the dedupe field stops a replay`;
  }
  return `HMAC-SHA256 of "timestamp.body" in ${hook.signature_header} (${prefix}), timestamp in ${hook.timestamp_header}, within ${formatDuration(hook.tolerance_seconds)}`;
}

/**
 * A shell command that signs and posts one payload the way the hook expects, for the person to
 * copy. The secret is written into the command only in the dialog that shows it once.
 */
export function curlExample(
  url: string,
  hook: Pick<Webhook, "signature_header" | "signature_prefix" | "timestamp_header">,
  secret: string,
): string {
  const lines = [`BODY='{"id":"order-1","email":"ada@example.com"}'`];
  if (hook.timestamp_header === null) {
    lines.push(`SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac '${secret}' | sed 's/^.* //')`);
  } else {
    lines.push("TS=$(date +%s)");
    lines.push(`SIG=$(printf '%s' "$TS.$BODY" | openssl dgst -sha256 -hmac '${secret}' | sed 's/^.* //')`);
  }
  const curl = [`curl -X POST '${url}'`, `  -H 'Content-Type: application/json'`];
  if (hook.timestamp_header !== null) curl.push(`  -H "${hook.timestamp_header}: $TS"`);
  curl.push(`  -H "${hook.signature_header}: ${hook.signature_prefix}$SIG"`, `  -d "$BODY"`);
  return [...lines, curl.join(" \\\n")].join("\n");
}

/** What a code the hooks API or a reject carries means, in plain words. */
export function webhookCodeText(code: string): string {
  const texts: Record<string, string> = {
    invalid_signature: "The signature or its timestamp did not check out.",
    invalid_payload: "The body was not one JSON object.",
    dedupe_missing: "The payload has no usable value at the dedupe field.",
    managed_by_webhooks: "A webhook source is changed under Webhooks, not as a plain source.",
    push_source: "A webhook source receives records as they are sent; it has no run.",
    sample_required: "Paste a sample payload to preview a webhook stream.",
    sample_not_supported: "Only a webhook stream previews a pasted sample.",
    key_column_not_supported: "A webhook stream has no key column; the dedupe field identifies a payload.",
    delete_signal_unsupported: "A webhook stream has no delete signal.",
    mapping_unavailable: "The stream had no usable mapping when the payload was applied.",
  };
  return texts[code] ?? code;
}
