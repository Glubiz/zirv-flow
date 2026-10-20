import type { components } from "../../api";
import type { Graph } from "./model";

export type TestRequest = components["schemas"]["FlowTestRequest"];
export type TestResult = components["schemas"]["FlowTestBody"];
export type PathStep = components["schemas"]["PathStepBody"];
export type ScriptStep = components["schemas"]["ScriptStepBody"];
export type Snapshot = components["schemas"]["SnapshotBody"];
export type Effect = components["schemas"]["EffectBody"];

export const UNITS = [
  { id: "minutes", label: "minutes", seconds: 60 },
  { id: "hours", label: "hours", seconds: 3600 },
  { id: "days", label: "days", seconds: 86_400 },
] as const;

export type UnitId = (typeof UNITS)[number]["id"];

/**
 * The request that walks a person with this script over this graph, the draft as it is on the canvas, saved or not.
 * The first walk names the person; every later one re-posts the snapshot the last response returned.
 */
export function walkRequest(
  who: { personId: string } | { snapshot: Snapshot },
  startAt: string | null,
  script: readonly ScriptStep[],
  last: TestResult | null,
  graph: Graph,
): TestRequest {
  if (last) {
    return {
      graph,
      snapshot: last.snapshot,
      start_at: last.start_at,
      enrollment_id: last.enrollment_id,
      script: [...script],
    };
  }
  return {
    graph,
    ...("personId" in who ? { person_id: who.personId } : { snapshot: who.snapshot }),
    ...(startAt ? { start_at: startAt } : {}),
    script: [...script],
  };
}

/** A `datetime-local` value read as UTC, as an instant; null when it is empty or not a time. */
export function utcInstant(value: string): string | null {
  if (value.trim() === "") return null;
  const date = new Date(`${value}${value.length === 16 ? ":00" : ""}Z`);
  return Number.isNaN(date.getTime()) ? null : date.toISOString();
}

/** The clock moved on by an amount of one unit. */
export function advanceBy(clock: string, amount: number, unit: UnitId): string {
  const seconds = UNITS.find((entry) => entry.id === unit)?.seconds ?? 60;
  return new Date(new Date(clock).getTime() + amount * seconds * 1000).toISOString();
}

export function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Parses the text of a JSON object field; the problem in words when it is not one. */
export function parseObject(text: string): { value: Record<string, unknown> } | { problem: string } {
  const trimmed = text.trim();
  if (trimmed === "") return { value: {} };
  try {
    const parsed: unknown = JSON.parse(trimmed);
    return isObject(parsed) ? { value: parsed } : { problem: "This must be a JSON object, such as {\"plan\": \"pro\"}." };
  } catch {
    return { problem: "This is not valid JSON." };
  }
}

export function pretty(value: unknown): string {
  return JSON.stringify(value, null, 2);
}
