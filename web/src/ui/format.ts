export function formatDate(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleDateString();
}

export function formatDateTime(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

export function formatDuration(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined) return "—";
  if (seconds < 1) return "under 1 s";
  if (seconds < 60) return `${Math.round(seconds)} s`;
  if (seconds < 3600) return `${Math.round(seconds / 60)} min`;
  if (seconds < 86400) return `${(seconds / 3600).toFixed(1)} h`;
  return `${(seconds / 86400).toFixed(1)} d`;
}

export function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

export function shortId(id: string): string {
  return id.length > 13 ? `${id.slice(0, 4)}…${id.slice(-4)}` : id;
}

/** How long ago a moment was, in words: "3 hours ago". A moment in the future reads as "just now". */
export function formatAgo(value: string, now: number = Date.now()): string {
  const then = new Date(value).getTime();
  if (Number.isNaN(then)) return value;
  const seconds = Math.max(0, Math.floor((now - then) / 1000));
  if (seconds < 60) return "just now";
  const unit = (count: number, name: string) => `${count} ${name}${count === 1 ? "" : "s"} ago`;
  if (seconds < 3600) return unit(Math.floor(seconds / 60), "minute");
  if (seconds < 86400) return unit(Math.floor(seconds / 3600), "hour");
  return unit(Math.floor(seconds / 86400), "day");
}

export function formatTime(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

/** A count for a figure: separators under 100,000, "2.4 M" above. */
export function formatCount(value: number): string {
  const compact = (divisor: number, unit: string) => `${(value / divisor).toFixed(1).replace(/\.0$/, "")} ${unit}`;
  if (Math.abs(value) >= 1_000_000) return compact(1_000_000, "M");
  if (Math.abs(value) >= 100_000) return compact(1_000, "k");
  return value.toLocaleString("en-US");
}

/** A calendar day (`2026-10-01`) as "1 Oct". Days are UTC, so the zone is fixed. */
export function formatDay(value: string): string {
  const date = new Date(`${value}T00:00:00Z`);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleDateString("en-GB", { day: "numeric", month: "short", timeZone: "UTC" });
}
