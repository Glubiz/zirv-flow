// @vitest-environment node
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

const css = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "styles.css"), "utf8");

/** The declarations of the first rule whose selector is exactly `selector`. */
function rule(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = new RegExp(`(?:^|\\})\\s*${escaped}\\s*\\{([^}]*)\\}`, "m").exec(css);
  return match?.[1] ?? "";
}

describe("narrow tables", () => {
  it("lets a monospace key column keep whole words, so the table scrolls instead of wrapping it one character per line", () => {
    expect(rule(".table td.mono")).toContain("overflow-wrap: break-word");
  });

  it("turns a table of cards into stacked rows below 640px", () => {
    const start = css.indexOf("@media (max-width: 640px) {\n  .table-cards");
    expect(start).toBeGreaterThan(-1);
    const block = css.slice(start, css.indexOf("\n}\n", start));
    expect(block).toContain(".table-cards tr");
    expect(block).toContain("attr(data-label)");
  });
});

describe("phone width", () => {
  it("makes a table scroller the containing block of hidden cell text, so it cannot widen the page", () => {
    expect(rule(".table-scroll")).toContain("position: relative");
  });

  it("keeps a segmented control inside its container and scrolls it there", () => {
    const declarations = rule(".seg-tabs");
    expect(declarations).toContain("max-width: 100%");
    expect(declarations).toContain("overflow-x: auto");
    expect(rule(".seg")).toContain("flex: none");
  });
});

describe("the sidebar list", () => {
  it("shows that it scrolls with a thin scrollbar and edge shadows", () => {
    const declarations = rule(".sidebar-nav");
    expect(declarations).toContain("scrollbar-width: thin");
    expect(declarations).toContain("radial-gradient");
  });
});
