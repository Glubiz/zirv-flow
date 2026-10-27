export type WindowUnit = "days" | "months";

const DAYS_PER_MONTH = 365 / 12;

/** Days in `months` months, with a month of 365/12 days rounded to the nearest day. */
export function monthsToDays(months: number): number {
  return Math.round(months * DAYS_PER_MONTH);
}

/** The whole number of months that `days` is exactly, or null when it is not one (or is a single month). */
export function wholeMonths(days: number): number | null {
  const months = Math.round(days / DAYS_PER_MONTH);
  return months >= 2 && monthsToDays(months) === days ? months : null;
}

/** A window in the largest whole unit: years, then months, then days. */
export function formatWindow(days: number): string {
  const months = wholeMonths(days);
  if (months === null) {
    return `${days} ${days === 1 ? "day" : "days"}`;
  }
  if (months % 12 === 0) {
    const years = months / 12;
    return `${years} ${years === 1 ? "year" : "years"}`;
  }
  return `${months} months`;
}

/** A value and unit as days. */
export function toDays(amount: number, unit: WindowUnit): number {
  return unit === "months" ? monthsToDays(amount) : amount;
}
