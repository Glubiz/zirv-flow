import { describe, expect, it } from "vitest";

import { subjectHref, traceHref } from "./href";

describe("traceHref", () => {
  it("points at the trace page of the workspace", () => {
    expect(traceHref("ws1", "0123456789abcdef0123456789abcdef")).toBe("/w/ws1/traces/0123456789abcdef0123456789abcdef");
  });
});

describe("subjectHref", () => {
  it("opens a message in the message log of its person, and has nowhere to open one without a person", () => {
    expect(subjectHref("ws1", "message", "m1", { message: "m1", person: "p1" })).toEqual({
      href: "/w/ws1/people/p1?tab=messages",
      text: "Messages of this person",
    });
    expect(subjectHref("ws1", "message", "m1", { message: "m1" })).toBeNull();
  });

  it("links a person and a flow, and shows a destination as text until it has a page", () => {
    expect(subjectHref("ws1", "person", "p1", {})?.href).toBe("/w/ws1/people/p1");
    expect(subjectHref("ws1", "flow", "f1", {})?.href).toBe("/w/ws1/flows/f1");
    expect(subjectHref("ws1", "destination", "d1", {})).toBeNull();
  });
});
