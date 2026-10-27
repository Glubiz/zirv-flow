import type { ErrorSummary } from "../../api";
import type { TemplateIssue, TemplatePreview } from "./model";
import { isBlocking } from "./model";

/** The most previews the page asks for at once. */
export const MAX_IN_FLIGHT = 3;

/** Runs tasks with at most `max` in flight; the rest wait their turn, in order. */
export type Limiter = <T>(task: () => Promise<T>) => Promise<T>;

export function createLimiter(max: number): Limiter {
  let running = 0;
  const waiting: (() => void)[] = [];
  const next = () => {
    running -= 1;
    waiting.shift()?.();
  };
  return async <T>(task: () => Promise<T>): Promise<T> => {
    if (running >= max) await new Promise<void>((resolve) => waiting.push(resolve));
    running += 1;
    try {
      return await task();
    } finally {
      next();
    }
  };
}

/** The sample person, or a saved person by id. */
export interface Subject {
  /** `sample` or the person id. */
  key: string;
  label: string;
  personId: string | null;
}

export const SAMPLE_SUBJECT: Subject = { key: "sample", label: "the sample person", personId: null };

export type Check = { status: "running" } | { status: "ready"; preview: TemplatePreview } | { status: "failed"; error: ErrorSummary };

export interface Summary {
  /** Every check has answered. */
  done: boolean;
  /** Checks that could not run. */
  failed: number;
  blocking: TemplateIssue[];
  warnings: TemplateIssue[];
  /** The people whose preview rendered with nothing blocking. */
  passed: number;
}

const issueKey = (issue: TemplateIssue) => `${issue.code}|${issue.at}|${issue.field ?? ""}`;

/** The issues across all people, each once, and how many people passed. */
export function summarize(subjects: readonly Subject[], checks: Readonly<Record<string, Check | undefined>>): Summary {
  const seen = new Set<string>();
  const blocking: TemplateIssue[] = [];
  const warnings: TemplateIssue[] = [];
  let failed = 0;
  let passed = 0;
  let done = true;
  for (const subject of subjects) {
    const check = checks[subject.key];
    if (check === undefined || check.status === "running") {
      done = false;
      continue;
    }
    if (check.status === "failed") {
      failed += 1;
      continue;
    }
    if (!check.preview.issues.some(isBlocking)) passed += 1;
    for (const issue of check.preview.issues) {
      if (seen.has(issueKey(issue))) continue;
      seen.add(issueKey(issue));
      (isBlocking(issue) ? blocking : warnings).push(issue);
    }
  }
  return { done, failed, blocking, warnings, passed };
}
