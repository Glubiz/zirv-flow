import { describe, expect, it } from "vitest";

import { traceHref } from "./href";

describe("traceHref", () => {
  it("points at the trace page of the workspace", () => {
    expect(traceHref("ws1", "0123456789abcdef0123456789abcdef")).toBe("/w/ws1/traces/0123456789abcdef0123456789abcdef");
  });
});
