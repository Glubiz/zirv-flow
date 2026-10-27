import type { components } from "../../api";

export type AuditEntry = components["schemas"]["AuditEntryBody"];

export interface Filters {
  action: string;
  actorKind: string;
  actorId: string;
  objectKind: string;
  objectId: string;
  from: string;
  to: string;
  requestId: string;
}

export const NO_FILTERS: Filters = {
  action: "",
  actorKind: "",
  actorId: "",
  objectKind: "",
  objectId: "",
  from: "",
  to: "",
  requestId: "",
};

const PARAMS: Record<keyof Filters, string> = {
  action: "action",
  actorKind: "actor_kind",
  actorId: "actor_id",
  objectKind: "object_kind",
  objectId: "object_id",
  from: "from",
  to: "to",
  requestId: "request_id",
};

export const ACTOR_KINDS = [
  { value: "user", label: "User" },
  { value: "api_key", label: "API key" },
  { value: "system", label: "System" },
  { value: "anonymous", label: "Anonymous" },
];

export function filtersFromParams(params: URLSearchParams): Filters {
  const read = (key: keyof Filters) => (params.get(PARAMS[key]) ?? "").trim();
  return {
    action: read("action"),
    actorKind: read("actorKind"),
    actorId: read("actorId"),
    objectKind: read("objectKind"),
    objectId: read("objectId"),
    from: read("from"),
    to: read("to"),
    requestId: read("requestId"),
  };
}

/** The URL query of a view: only the filters that are set, plus `scope=instance` for the owner's instance rows. */
export function paramsFromFilters(filters: Filters, instance: boolean): URLSearchParams {
  const params = new URLSearchParams();
  for (const key of Object.keys(PARAMS) as (keyof Filters)[]) {
    const value = filters[key].trim();
    if (value !== "") params.set(PARAMS[key], value);
  }
  if (instance) params.set("scope", "instance");
  return params;
}

function localMidnight(date: string, plusDays: number): string | undefined {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(date);
  if (!match) return undefined;
  const moment = new Date(Number(match[1]), Number(match[2]) - 1, Number(match[3]) + plusDays);
  return Number.isNaN(moment.getTime()) ? undefined : moment.toISOString();
}

export interface AuditQuery {
  action?: string;
  actor_kind?: string;
  actor_id?: string;
  object_kind?: string;
  object_id?: string;
  from?: string;
  to?: string;
  request_id?: string;
}

/** The API query of the filters. The dates are whole local days: From is the start of its day, To the start of the day after, since the API's To is exclusive. */
export function queryFromFilters(filters: Filters): AuditQuery {
  const query: AuditQuery = {};
  if (filters.action) query.action = filters.action;
  if (filters.actorKind) query.actor_kind = filters.actorKind;
  if (filters.actorId) query.actor_id = filters.actorId;
  if (filters.objectKind) query.object_kind = filters.objectKind;
  if (filters.objectId) query.object_id = filters.objectId;
  const from = localMidnight(filters.from, 0);
  if (from) query.from = from;
  const to = localMidnight(filters.to, 1);
  if (to) query.to = to;
  if (filters.requestId) query.request_id = filters.requestId;
  return query;
}

export type ExportFormat = "ndjson" | "csv";

export function exportUrl(workspaceId: string, instance: boolean, filters: Filters, format: ExportFormat): string {
  const params = new URLSearchParams({ format });
  const query: Record<string, string | undefined> = { ...queryFromFilters(filters) };
  for (const [key, value] of Object.entries(query)) if (value !== undefined) params.set(key, value);
  const base = instance ? "/api/v1/audit/export" : `/api/v1/workspaces/${workspaceId}/audit/export`;
  return `${base}?${params.toString()}`;
}

export function hasFilters(filters: Filters): boolean {
  return Object.values(filters).some((value) => value !== "");
}

function isPair(value: unknown): value is { from: unknown; to: unknown } {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const keys = Object.keys(value);
  return keys.length === 2 && keys.includes("from") && keys.includes("to");
}

/** A value as compact text on one line: `from -> to` for a pair, `a, b` for a list, `k: v` for a nested object. */
export function compact(value: unknown): string {
  if (value === null || value === undefined) return "none";
  if (typeof value === "string") return value === "" ? "empty" : value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (Array.isArray(value)) return value.length === 0 ? "none" : value.map(compact).join(", ");
  if (isPair(value)) return `${compact(value.from)} -> ${compact(value.to)}`;
  const entries = Object.entries(value as Record<string, unknown>);
  return entries.length === 0
    ? "none"
    : entries.map(([key, inner]: [string, unknown]) => `${key.replace(/_/g, " ")}: ${compact(inner)}`).join(", ");
}

export interface Change {
  key: string;
  label: string;
  value: string;
}

const CHANGE_LABELS: Record<string, string> = {
  view_pii: "Personal data",
  role: "Role",
  scopes: "Scopes",
};

/** The label of a summary key: a known key reads in plain words, any other is derived from the key. */
export function changeLabel(key: string): string {
  return CHANGE_LABELS[key] ?? key.replace(/_/g, " ").replace(/^./, (first) => first.toUpperCase());
}

/** The summary of an entry as key-value rows. */
export function changesOf(summary: unknown): Change[] {
  if (typeof summary !== "object" || summary === null || Array.isArray(summary)) return [];
  return Object.entries(summary as Record<string, unknown>).map(([key, value]) => ({
    key,
    label: changeLabel(key),
    value: compact(value),
  }));
}

const PAGES: Record<string, string> = {
  flow: "flows",
  segment: "segments",
  list: "lists",
  template: "templates",
  theme: "themes",
  snippet: "snippets",
  source: "sources",
  import: "imports",
  destination: "destinations",
  sending_domain: "sending-domains",
};

/** Where an object leads, or none: a person is an opaque id only, and an object that was deleted, removed or erased has no page, while an archived one still does. */
export function objectHref(workspaceId: string, entry: AuditEntry): string | null {
  const kind = entry.object_kind;
  const id = entry.object_id;
  if (!kind || !id) return null;
  if (/\.(deleted|removed|erased)$/.test(entry.action)) return null;
  const page = PAGES[kind];
  return page ? `/w/${workspaceId}/${page}/${id}` : null;
}
