import { describe, expect, it } from "vitest";

import { TRACKED, asLines, normaliseTracking, unifiedDiff } from "./tracking";

const forms = { publicUrl: "https://flow.example.test", direct: ["https://shop.example/spring"] };

describe("normaliseTracking", () => {
  it("maps a click token in an href", () => {
    const html = '<a href="https://flow.example.test/t/c/abc123_-X">Go</a>';
    expect(normaliseTracking(html, forms)).toBe(`<a href="${TRACKED}">Go</a>`);
  });

  it("maps the unsubscribe token in an href, in the escaped form the renderer writes, and in a text body", () => {
    const html = '<a href="https:&#x2f;&#x2f;flow.example.test&#x2f;u&#x2f;tok-en">Unsubscribe</a>';
    expect(normaliseTracking(html, forms)).toBe(`<a href="${TRACKED}">Unsubscribe</a>`);
    expect(normaliseTracking("Unsubscribe: https://flow.example.test/u/tok-en\n", forms)).toBe(`Unsubscribe: ${TRACKED}\n`);
  });

  it("maps the inert preview addresses in an href and in a text body", () => {
    expect(normaliseTracking('<a href="https://preview.sample.invalid/t/inert">Go</a>', forms)).toBe(`<a href="${TRACKED}">Go</a>`);
    expect(normaliseTracking('<a href="https://preview.sample.invalid/t">Go</a>', forms)).toBe(`<a href="${TRACKED}">Go</a>`);
    expect(normaliseTracking('<a href="https://unsubscribe.sample.invalid/u">U</a>', forms)).toBe(`<a href="${TRACKED}">U</a>`);
    expect(normaliseTracking("Unsubscribe: https://unsubscribe.sample.invalid/u", forms)).toBe(`Unsubscribe: ${TRACKED}`);
  });

  it("drops the open pixel, whatever its attribute order", () => {
    const pixel = '<img src="https://flow.example.test/t/o/tok" width="1" height="1" alt="">';
    expect(normaliseTracking(`<p>Hi</p>${pixel}`, forms)).toBe("<p>Hi</p>");
    expect(normaliseTracking('<img alt="" width="1" src="https://flow.example.test/t/o/tok">', forms)).toBe("");
    expect(normaliseTracking('<img src="https:&#x2f;&#x2f;flow.example.test&#x2f;t&#x2f;o&#x2f;tok">', forms)).toBe("");
    expect(normaliseTracking('<p>Hi</p><img src="https://preview.sample.invalid/t/o/inert" width="1" height="1" alt="">', forms)).toBe("<p>Hi</p>");
  });

  it("treats a direct address as the tracked link only inside an href", () => {
    expect(normaliseTracking('<a href="https://shop.example/spring">Go</a>', forms)).toBe(`<a href="${TRACKED}">Go</a>`);
    const text = "See https://shop.example/spring";
    expect(normaliseTracking(text, forms)).toBe(text);
  });

  it("leaves every other address, and every other difference, alone", () => {
    const html = '<a href="https://shop.example/t/offer">A</a><img src="https://flow.example.test/a/logo.png"><a href="https://shop.example/spring2">B</a>';
    expect(normaliseTracking(html, forms)).toBe(html);
  });
});

describe("asLines", () => {
  it("reads CRLF as LF and drops trailing blank lines", () => {
    expect(asLines("a\r\nb\r\n\r\n")).toBe("a\nb");
  });

  it("keeps the spaces and tabs on the last content line", () => {
    expect(asLines("a\nb \t\r\n")).toBe("a\nb \t");
  });
});

describe("unifiedDiff", () => {
  it("is empty for equal text", () => {
    expect(unifiedDiff("<p>a</p>", "<p>a</p>")).toBe("");
  });

  it("shows the tag that differs, with context", () => {
    const diff = unifiedDiff('<p>a</p><td bgcolor="#111">b</td><p>c</p>', '<p>a</p><td bgcolor="#222">b</td><p>c</p>');
    expect(diff).toContain('-<td bgcolor="#111">');
    expect(diff).toContain('+<td bgcolor="#222">');
    expect(diff).toContain(" <p>a</p>");
  });
});
