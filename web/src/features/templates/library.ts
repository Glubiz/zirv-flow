import type { components } from "../../api";
import { formatAgo, plural } from "../../ui/format";

export type TemplateListItem = components["schemas"]["TemplateListItemBody"];

export type StatusFilter = "" | "live" | "changes" | "draft" | "archived";
export type ChannelFilter = "" | "email" | "webhook";
export type SortKey = "edited" | "name" | "most_used";
export type ViewMode = "cards" | "list";

/** What the library page shows, kept in the URL so a reload and a shared link keep it. */
export interface Filters {
  q: string;
  status: StatusFilter;
  channel: ChannelFilter;
  /** A theme id, `default` or `none`; empty for every theme. */
  theme: string;
  sort: SortKey;
  view: ViewMode;
}

export const NO_FILTERS: Filters = { q: "", status: "", channel: "", theme: "", sort: "edited", view: "cards" };

export const STATUS_CHIPS: readonly { id: StatusFilter; label: string }[] = [
  { id: "", label: "All" },
  { id: "live", label: "Live" },
  { id: "changes", label: "Draft changes" },
  { id: "draft", label: "Draft" },
  { id: "archived", label: "Archived" },
];

export const SORTS: readonly { id: SortKey; label: string }[] = [
  { id: "edited", label: "Last edited" },
  { id: "name", label: "Name" },
  { id: "most_used", label: "Most used" },
];

function oneOf<T extends string>(value: string | null, allowed: readonly T[], fallback: T): T {
  return allowed.find((entry) => entry === value) ?? fallback;
}

/** The filters in a URL query; a value this page does not know reads as its default. */
export function readFilters(params: URLSearchParams): Filters {
  return {
    q: params.get("q") ?? "",
    status: oneOf(params.get("status"), ["", "live", "changes", "draft", "archived"], ""),
    channel: oneOf(params.get("channel"), ["", "email", "webhook"], ""),
    theme: params.get("theme") ?? "",
    sort: oneOf(params.get("sort"), ["edited", "name", "most_used"], "edited"),
    view: oneOf(params.get("view"), ["cards", "list"], "cards"),
  };
}

/** The URL query for the filters; a default is left out. */
export function writeFilters(filters: Filters): URLSearchParams {
  const params = new URLSearchParams();
  for (const key of ["q", "status", "channel", "theme", "sort", "view"] as const) {
    if (filters[key] !== NO_FILTERS[key]) params.set(key, filters[key]);
  }
  return params;
}

/** Whether anything narrows the list. */
export function isFiltered(filters: Filters): boolean {
  return filters.q !== "" || filters.status !== "" || filters.channel !== "" || filters.theme !== "";
}

/** The query of the list call: the filters and the sort, nothing for a default. */
export function listParams(filters: Filters): Record<string, string> {
  const query: Record<string, string> = {};
  if (filters.q !== "") query["q"] = filters.q;
  if (filters.status !== "") query["status"] = filters.status;
  if (filters.channel !== "") query["channel"] = filters.channel;
  if (filters.theme !== "") query["theme"] = filters.theme;
  if (filters.sort !== "edited") query["sort"] = filters.sort;
  return query;
}

/** How many templates each status chip would list. "All" leaves out the archived ones, as the list does. */
export function statusCounts(items: readonly TemplateListItem[]): Record<StatusFilter, number> {
  const counts: Record<StatusFilter, number> = { "": 0, live: 0, changes: 0, draft: 0, archived: 0 };
  for (const item of items) {
    const status = statusKey(item);
    counts[status] += 1;
    if (status !== "archived") counts[""] += 1;
  }
  return counts;
}

/** Where a template is in its life. A published template with a changed draft is `changes`. */
export function statusKey(item: TemplateListItem): Exclude<StatusFilter, ""> {
  if (item.archived_at) return "archived";
  if (item.current_version === null || item.current_version === undefined) return "draft";
  return item.draft_changed ? "changes" : "live";
}

/** The status in words; a changed draft of a live template is also marked on its own. */
export function statusLabel(item: TemplateListItem): string {
  const key = statusKey(item);
  if (key === "archived") return "Archived";
  if (key === "draft") return "Draft";
  return "Live";
}

export function kindLabel(item: TemplateListItem): string {
  if (item.channel === "webhook") return "Webhook";
  return `Email · ${item.category === "transactional" ? "Transactional" : "Marketing"}`;
}

/** The result of the draft's last check, in words. */
export function checksLabel(item: TemplateListItem): { text: string; tone: "neutral" | "attention" | "error" } {
  const checks = item.checks;
  if (!checks) return { text: "Not checked", tone: "neutral" };
  if (checks.blocking > 0) return { text: plural(checks.blocking, "blocking issue", "blocking issues"), tone: "error" };
  if (checks.warnings > 0) return { text: plural(checks.warnings, "warning", "warnings"), tone: "attention" };
  return { text: "No issues", tone: "neutral" };
}

export function usedLabel(item: TemplateListItem): string {
  return item.active_flows === 0 ? "Not used by a flow" : `Used in ${flowCount(item)}`;
}

/** The column form of [`usedLabel`]. */
export function flowCount(item: TemplateListItem): string {
  return item.active_flows === 0 ? "Not used" : plural(item.active_flows, "flow", "flows");
}

/** The theme column: the workspace default and no theme are words, a named theme its name. */
export function themeLabel(item: TemplateListItem): string {
  if (item.theme === "default") return "Workspace default";
  if (item.theme === "none") return item.channel === "webhook" ? "None" : "No theme";
  return item.theme.name;
}

export function editedLabel(item: TemplateListItem, now?: number): string {
  return `Edited ${editedAgo(item, now)}`;
}

/** The column form of [`editedLabel`]. */
export function editedAgo(item: TemplateListItem, now?: number): string {
  return formatAgo(item.edited_at, now);
}

const MAX_NAME = 100;
const COPY_PREFIX = "Copy of ";

/** The names a copy of `name` can take, in order: "Copy of X", "Copy of X 2", "Copy of X 3" and so on. */
export function copyNames(name: string, count = 30): string[] {
  const base = `${COPY_PREFIX}${name}`;
  const names: string[] = [];
  for (let n = 1; n <= count; n += 1) {
    const suffix = n === 1 ? "" : ` ${n}`;
    names.push(`${base.slice(0, MAX_NAME - suffix.length).trimEnd()}${suffix}`);
  }
  return names;
}
