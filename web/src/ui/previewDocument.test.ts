import { describe, expect, it } from "vitest";

import { previewDocument } from "./previewDocument";

describe("previewDocument", () => {
  it.each([
    "",
    "<p>Preview</p>",
    '<!doctype html><html lang="da"><head><style>p { color: red }</style></head><body><p>Preview</p></body></html>',
    '<!-- <head> --><HEAD><base href="https://evil.example/"></HEAD><BODY><p>Preview</p>',
    '</head><script>window.__ran = true</script><meta http-equiv="Content-Security-Policy" content="default-src *">',
  ])("puts the CSP first in the parsed head for %s", (html) => {
    const source = previewDocument(html);
    const document = new DOMParser().parseFromString(source, "text/html");
    const first = document.head.firstChild as HTMLMetaElement;
    expect(first.tagName).toBe("META");
    expect(first.httpEquiv).toBe("Content-Security-Policy");
    expect(first.content).toBe("default-src 'none'; img-src https: data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'");
    expect(source).toContain(html);
    if (html.includes("<p>Preview</p>")) expect(document.body.textContent).toContain("Preview");
  });
});
