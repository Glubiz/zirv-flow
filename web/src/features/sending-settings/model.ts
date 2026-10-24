import type { components } from "../../api";

export type SendSettings = components["schemas"]["SendSettingsBody"];

export const MAX_CAPS = 20;
export const MAX_WINDOW_HOURS = 31 * 24;

export const WEEKDAYS = [
  { id: "mon", label: "Mon" },
  { id: "tue", label: "Tue" },
  { id: "wed", label: "Wed" },
  { id: "thu", label: "Thu" },
  { id: "fri", label: "Fri" },
  { id: "sat", label: "Sat" },
  { id: "sun", label: "Sun" },
] as const;

export type CapMode = "skip" | "defer";

/** One frequency cap as the form holds it: the window in hours and the limit as the text typed. */
export interface CapDraft {
  key: number;
  channel: string;
  hours: string;
  limit: string;
  mode: CapMode;
}

export interface QuietDraft {
  start: string;
  end: string;
  days: readonly string[];
}

export interface SettingsDraft {
  caps: CapDraft[];
  /** Null when there are no quiet hours. */
  quiet: QuietDraft | null;
}

let counter = 0;

export function nextCapKey(): number {
  counter += 1;
  return counter;
}

/** Whole hours in an ISO 8601 duration of days and hours, such as `P1D`, `PT12H` or `P1DT6H`; null otherwise. */
export function hoursOf(window: string): number | null {
  const match = /^P(?:(\d+)D)?(?:T(?:(\d+)H)?)?$/.exec(window);
  if (match === null || (match[1] === undefined && match[2] === undefined)) return null;
  return Number(match[1] ?? 0) * 24 + Number(match[2] ?? 0);
}

/** The shortest ISO 8601 form of a whole number of hours: days when it divides evenly. */
export function windowOf(hours: number): string {
  return hours % 24 === 0 ? `P${hours / 24}D` : `PT${hours}H`;
}

/** How a window reads: "1 day", "12 hours". */
export function windowText(hours: number): string {
  if (hours % 24 === 0) {
    const days = hours / 24;
    return days === 1 ? "1 day" : `${days} days`;
  }
  return hours === 1 ? "1 hour" : `${hours} hours`;
}

export function draftFromApi(settings: SendSettings): SettingsDraft {
  return {
    caps: (settings.frequency_caps ?? []).map((cap) => ({
      key: nextCapKey(),
      channel: cap.channel,
      hours: String(hoursOf(cap.window) ?? ""),
      limit: String(cap.limit),
      mode: cap.mode === "defer" ? "defer" : "skip",
    })),
    quiet: settings.quiet_hours ? { start: settings.quiet_hours.start, end: settings.quiet_hours.end, days: settings.quiet_hours.days } : null,
  };
}

/** The whole settings document, as the `PUT` replaces it. */
export function draftToApi(draft: SettingsDraft): SendSettings {
  return {
    frequency_caps: draft.caps.map((cap) => ({
      channel: cap.channel,
      window: windowOf(Number(cap.hours)),
      limit: Number(cap.limit),
      mode: cap.mode,
    })),
    quiet_hours: draft.quiet === null ? null : { start: draft.quiet.start, end: draft.quiet.end, days: [...draft.quiet.days] },
  };
}

export interface CapProblems {
  hours?: string;
  limit?: string;
}

export interface Problems {
  caps: Map<number, CapProblems>;
  quiet: { end?: string; days?: string };
}

/** What the form can tell is wrong before asking the server. The server stays the authority. */
export function check(draft: SettingsDraft): Problems {
  const problems: Problems = { caps: new Map(), quiet: {} };
  for (const cap of draft.caps) {
    const found: CapProblems = {};
    const hours = Number(cap.hours);
    if (cap.hours.trim() === "" || !Number.isInteger(hours) || hours < 1 || hours > MAX_WINDOW_HOURS) {
      found.hours = `Use a whole number of hours from 1 to ${MAX_WINDOW_HOURS} (31 days).`;
    }
    const limit = Number(cap.limit);
    if (cap.limit.trim() === "" || !Number.isInteger(limit) || limit < 1) {
      found.limit = "Use a whole number of at least 1.";
    }
    if (found.hours !== undefined || found.limit !== undefined) problems.caps.set(cap.key, found);
  }
  if (draft.quiet !== null) {
    if (draft.quiet.start === draft.quiet.end) problems.quiet.end = "The end has to differ from the start.";
    if (draft.quiet.days.length === 0) problems.quiet.days = "Choose at least one day.";
  }
  return problems;
}

export function hasProblems(problems: Problems): boolean {
  return problems.caps.size > 0 || problems.quiet.end !== undefined || problems.quiet.days !== undefined;
}

export function sameDraft(left: SettingsDraft, right: SettingsDraft): boolean {
  return JSON.stringify(draftToApiLoose(left)) === JSON.stringify(draftToApiLoose(right));
}

/** Like `draftToApi`, but safe for text that is not a number yet, so two drafts can be compared. */
function draftToApiLoose(draft: SettingsDraft) {
  return {
    caps: draft.caps.map((cap) => [cap.channel, cap.hours, cap.limit, cap.mode]),
    quiet: draft.quiet === null ? null : [draft.quiet.start, draft.quiet.end, [...draft.quiet.days].sort()],
  };
}
