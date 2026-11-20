import { describe, expect, it } from "vitest";

import { isMjml } from "./ImportDialog";

describe("isMjml", () => {
  it("reads a source that opens with an mjml tag as MJML", () => {
    expect(isMjml("<mjml><mj-body></mj-body></mjml>")).toBe(true);
    expect(isMjml("  \n<MJML lang='en'>")).toBe(true);
    expect(isMjml('<?xml version="1.0"?>\n<mjml>')).toBe(true);
    expect(isMjml("<!-- a --> <!-- b -->\n<!-- c --><mjml>")).toBe(true);
  });

  it("reads a byte-order-marked MJML document as MJML", () => {
    expect(isMjml("﻿<mjml><mj-body></mj-body></mjml>")).toBe(true);
  });

  it("reads anything else as HTML", () => {
    expect(isMjml("<html><body></body></html>")).toBe(false);
    expect(isMjml("<mjmlish>")).toBe(false);
    expect(isMjml("<!-- never closed <mjml>")).toBe(false);
    expect(isMjml("")).toBe(false);
  });

  it("answers a long run of comments before non-MJML at once", () => {
    const source = "<!---->".repeat(26) + "<html>";
    const start = performance.now();
    const result = isMjml(source);
    const elapsed = performance.now() - start;
    console.info(`isMjml(26 comments + <html>): ${elapsed.toFixed(2)} ms`);

    expect(result).toBe(false);
    expect(elapsed).toBeLessThan(100);
  });

  it("stays fast on a very long run of comments", () => {
    const start = performance.now();
    expect(isMjml("<!---->".repeat(20_000) + "<html>")).toBe(false);
    expect(isMjml("<!---->".repeat(20_000) + "<mjml>")).toBe(true);
    expect(performance.now() - start).toBeLessThan(200);
  });
});
