import type { components } from "../../api";
import { formatAgo, formatDuration } from "../../ui";

import { parseQueries, queriesValid } from "./queries";

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
    source_kind_unavailable: "This build of the server does not include this type of source.",
  };
  return texts[code] ?? `The run failed (${code}).`;
}

/**
 * Whether the values that have a shape of their own hold something the server will accept the
 * shape of: for now, the queries of a database source. Blocks saving a form edited into nonsense.
 */
export function shapesValid(fields: FieldSpec[], values: Values): boolean {
  return fields.every((field) => field.kind !== "queries" || queriesValid(values[field.key] ?? ""));
}

export type Freshness = components["schemas"]["FreshnessBody"];

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
