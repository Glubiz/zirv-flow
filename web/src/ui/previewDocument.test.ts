import { describe, expect, it } from "vitest";

import { PREVIEW_CSP_META, previewDocument } from "./previewDocument";

function expectPolicyFirst(source: string) {
  const document = new DOMParser().parseFromString(source, "text/html");
  const first = document.head.firstElementChild;
  expect(first?.localName).toBe("meta");
  expect(first?.getAttribute("http-equiv")).toBe("Content-Security-Policy");
  expect(first?.getAttribute("content")).toBe("default-src 'none'; img-src https: data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'");
  if (source.includes("<p>Preview</p>")) expect(document.body.textContent).toContain("Preview");
}

describe("previewDocument", () => {
  it.each([
    '<!doctype html><html lang="da"><head><style>p { color: red }</style></head><body><p>Preview</p></body></html>',
    '<!DOCTYPE html>\n<html lang="da"><HeAd data-label="a > b">\n<title>Preview</title></HeAd><body><p>Preview</p></body></html>',
    '<!-- <head> --><HEAD><base href="https://evil.example/"></HEAD><BODY><p>Preview</p>',
  ])("inserts only the CSP meta into an existing head: %s", (html) => {
    const source = previewDocument(html);
    expectPolicyFirst(source);
    expect(source.replace(PREVIEW_CSP_META, "")).toBe(html);
  });

  it.each([
    '<!doctype html><html lang="da"><body><p>Preview</p></body></html>',
    "<!DOCTYPE html>\n<HTML lang='da' data-label='a > b'>\n<BODY><p>Preview</p></BODY></HTML>",
    '<!-- <head> --><html><body><p>Preview</p></body></html>',
  ])("adds only a head with the CSP after an existing html tag: %s", (html) => {
    const source = previewDocument(html);
    expectPolicyFirst(source);
    expect(source.replace(`<head>${PREVIEW_CSP_META}</head>`, "")).toBe(html);
  });

  it.each([
    "",
    "<p>Preview</p>",
    '</head><script>window.__ran = true</script><meta http-equiv="Content-Security-Policy" content="default-src *">',
  ])("wraps a fragment unchanged with the CSP first in the head: %s", (html) => {
    const source = previewDocument(html);
    expectPolicyFirst(source);
    expect(source).toBe(`<!doctype html><html><head>${PREVIEW_CSP_META}</head><body>${html}</body></html>`);
  });
});
