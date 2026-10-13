const MICROS = 1_000_000;

/** Micro-units of the deployment currency as a plain amount: at least two decimals, at most four. */
export function formatMicros(micros: number): string {
  const [whole = "0", fraction = ""] = (micros / MICROS).toFixed(4).split(".");
  const trimmed = fraction.replace(/0+$/, "").padEnd(2, "0");
  return `${whole}.${trimmed}`;
}

/** The whole-unit amount an admin types, as micro-units; null when it is not a non-negative number. */
export function toMicros(text: string): number | null {
  if (text.trim() === "") return null;
  const value = Number(text);
  if (!Number.isFinite(value) || value < 0) return null;
  return Math.round(value * MICROS);
}

/** Micro-units as the text of an amount field. */
export function fromMicros(micros: number): string {
  return String(micros / MICROS);
}
