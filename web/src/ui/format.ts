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
