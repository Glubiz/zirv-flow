import { describe, expect, it } from "vitest";

import type { TemplateIssue, TemplatePreview } from "./model";
import { createLimiter, SAMPLE_SUBJECT, summarize, type Check, type Subject } from "./reviewChecks";

const issue = (code: string, severity: TemplateIssue["severity"], at = "/blocks/block/b1/alt"): TemplateIssue => ({ code, severity, at });
const ready = (...issues: TemplateIssue[]): Check => ({ status: "ready", preview: { issues } as unknown as TemplatePreview });
const person = (key: string): Subject => ({ key, label: key, personId: key });

describe("createLimiter", () => {
  it("never has more than the limit in flight and runs the rest in order", async () => {
    const limit = createLimiter(3);
    let running = 0;
    let peak = 0;
    const order: number[] = [];
    const tasks = Array.from({ length: 8 }, (_, index) =>
      limit(async () => {
        running += 1;
        peak = Math.max(peak, running);
        order.push(index);
        await new Promise((resolve) => setTimeout(resolve, 5));
        running -= 1;
        return index;
      }),
    );

    expect(await Promise.all(tasks)).toEqual([0, 1, 2, 3, 4, 5, 6, 7]);
    expect(peak).toBe(3);
    expect(order).toEqual([0, 1, 2, 3, 4, 5, 6, 7]);
  });

  it("frees its place when a task fails", async () => {
    const limit = createLimiter(1);

    await expect(limit(() => Promise.reject(new Error("no")))).rejects.toThrow("no");
    expect(await limit(() => Promise.resolve("next"))).toBe("next");
  });
});

describe("summarize", () => {
  it("counts the people who pass and lists each issue once, blocking apart from warnings", () => {
    const subjects = [SAMPLE_SUBJECT, person("a"), person("b")];
    const summary = summarize(subjects, {
      sample: ready(issue("image_without_alt", "warning")),
      a: ready(issue("image_without_alt", "warning"), issue("invalid_url", "blocking", "/blocks/block/b2/href")),
      b: ready(),
    });

    expect(summary.done).toBe(true);
    expect(summary.passed).toBe(2);
    expect(summary.warnings.map((entry) => entry.code)).toEqual(["image_without_alt"]);
    expect(summary.blocking.map((entry) => entry.code)).toEqual(["invalid_url"]);
  });

  it("is not done while a check runs, and counts a failed one", () => {
    const subjects = [SAMPLE_SUBJECT, person("a"), person("b")];

    const summary = summarize(subjects, { sample: ready(), a: { status: "failed", error: { title: "x", detail: "", traceId: null } } });

    expect(summary).toMatchObject({ done: false, failed: 1, passed: 1 });
  });
});
