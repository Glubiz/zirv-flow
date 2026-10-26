import { describe, expect, it } from "vitest";

import type { Change } from "./diff";
import { outlineStyle, withOutlines } from "./outline";

function change(tag: Change["tag"], markId: string | undefined): Change {
  return { key: `k:${markId ?? "none"}`, tag, where: "Section 1", what: "x", ...(markId === undefined ? {} : { markId }) };
}

describe("outlineStyle", () => {
  it("outlines added in green and changed in amber, keyed on the block class", () => {
    const style = outlineStyle([change("Added", "intro_1"), change("Changed", "cta-2")]);

    expect(style).toContain(".zf-block-intro_1{outline:2px solid #2e9d5b");
    expect(style).toContain(".zf-block-cta-2{outline:2px solid #d99a1e");
  });

  it("never lets a block id with a quote, a brace or a space reach the style", () => {
    const hostile = ['a"b', "a}b", "a{b", "a b", "a;b", "a</style>b", "a\b", ""];

    const style = outlineStyle([...hostile.map((id) => change("Changed", id)), change("Changed", "ok")]);

    expect(style).toBe("<style>.zf-block-ok{outline:2px solid #d99a1e;outline-offset:4px;border-radius:3px}</style>");
  });

  it("is empty when no change marks a block", () => {
    expect(outlineStyle([change("Changed", undefined)])).toBe("");
  });
});

describe("withOutlines", () => {
  it("puts the style in the head, or in front of a fragment", () => {
    const marks = [change("Added", "a")];

    expect(withOutlines("<html><head><title>t</title></head><body></body></html>", marks)).toMatch(/<style>.*<\/style><\/head>/);
    expect(withOutlines("<p>x</p>", marks).startsWith("<style>")).toBe(true);
    expect(withOutlines("<p>x</p>", [])).toBe("<p>x</p>");
  });
});
