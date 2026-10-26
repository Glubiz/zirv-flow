import { describe, expect, it } from "vitest";

import { DEFAULT_STYLE, type Block, type Section } from "./blocks/document";
import { diffDocuments } from "./diff";

function heading(id: string, text: string): Block {
  return { type: "heading", id, level: 1, text };
}
function text(id: string, content: string): Block {
  return { type: "text", id, content };
}
function button(id: string, label: string): Block {
  return { type: "button", id, label, url: "https://shop.example/a", align: "center" };
}
function section(id: string, ...blocks: Block[]): Section {
  return { id, columns: [blocks] };
}

function doc(sections: Section[], overrides: Record<string, unknown> = {}) {
  return {
    schema: 2,
    channel: "email",
    mode: "blocks",
    blocks: { subject: "Hello", preheader: "", category: "marketing", theme: "default", footer: "theme", settings: { ...DEFAULT_STYLE }, sections, ...overrides },
  };
}

const LIVE = () => doc([section("intro", heading("h1", "Welcome"), text("t1", "<p>Body</p>")), section("cta", button("b1", "Open"))]);

describe("diffDocuments", () => {
  it("reports nothing when nothing changed", () => {
    expect(diffDocuments(LIVE(), LIVE())).toEqual([]);
  });

  it("reports an added block with its place and marks it by id", () => {
    const draft = doc([section("intro", heading("h1", "Welcome"), text("t1", "<p>Body</p>"), text("t2", "<p>New</p>")), section("cta", button("b1", "Open"))]);

    expect(diffDocuments(LIVE(), draft)).toEqual([{ key: "block:t2", tag: "Added", where: "Section 1 › Text", what: "New text", markId: "t2" }]);
  });

  it("reports a changed block by its id, with the old and new short text", () => {
    const draft = doc([section("intro", heading("h1", "Welcome"), text("t1", "<p>Body</p>")), section("cta", button("b1", "Track your parcel"))]);

    expect(diffDocuments(LIVE(), draft)).toEqual([{ key: "block:b1", tag: "Changed", where: "Section 2 › Button", what: "Open → Track your parcel", markId: "b1" }]);
  });

  it("names the fields of a block whose longer content changed", () => {
    const draft = doc([section("intro", heading("h1", "Welcome"), text("t1", "<p>Other body</p>")), section("cta", button("b1", "Open"))]);

    expect(diffDocuments(LIVE(), draft)).toMatchObject([{ tag: "Changed", where: "Section 1 › Text", what: "Text changed", markId: "t1" }]);
  });

  it("reports a removed block and a removed section", () => {
    const draft = doc([section("intro", heading("h1", "Welcome"))]);

    expect(diffDocuments(LIVE(), draft)).toEqual([
      { key: "section:cta", tag: "Removed", where: "Section 2", what: "Section removed with 1 block", markId: "cta" },
      { key: "block:t1", tag: "Removed", where: "Section 1 › Text", what: "Text removed", markId: "t1" },
    ]);
  });

  it("tells the blocks of a new section with the section", () => {
    const draft = doc([...LIVE().blocks.sections, section("extra", text("t9", "<p>x</p>"), text("t10", "<p>y</p>"))]);

    expect(diffDocuments(LIVE(), draft)).toEqual([{ key: "section:extra", tag: "Added", where: "Section 3", what: "New section with 2 blocks", markId: "extra" }]);
  });

  it("reports a moved block as changed position, not as a removal plus an addition", () => {
    const draft = doc([section("intro", text("t1", "<p>Body</p>"), heading("h1", "Welcome")), section("cta", button("b1", "Open"))]);

    const changes = diffDocuments(LIVE(), draft);

    expect(changes).toHaveLength(1);
    expect(changes[0]).toMatchObject({ tag: "Changed", markId: "h1", what: "Moved within the section" });
    expect(changes.some((change) => change.tag === "Added" || change.tag === "Removed")).toBe(false);
  });

  it("reports a block moved to another section, and one that moved and changed", () => {
    const draft = doc([section("intro", heading("h1", "Welcome")), section("cta", text("t1", "<p>Body</p>"), button("b1", "Go"))]);

    expect(diffDocuments(LIVE(), draft)).toMatchObject([
      { markId: "t1", tag: "Changed", what: "Moved from section 1 to section 2" },
      { markId: "b1", tag: "Changed", what: "Open → Go" },
    ]);
  });

  it("does not call a block moved because something was added before it", () => {
    const draft = doc([section("intro", text("t0", "<p>First</p>"), heading("h1", "Welcome"), text("t1", "<p>Body</p>")), section("cta", button("b1", "Open"))]);

    expect(diffDocuments(LIVE(), draft).map((change) => change.markId)).toEqual(["t0"]);
  });

  it("reports a subject change with the old and new subject", () => {
    const draft = doc(LIVE().blocks.sections, { subject: "Hi there" });

    expect(diffDocuments(LIVE(), draft)).toEqual([{ key: "subject", tag: "Changed", where: "Subject", what: "Hello → Hi there" }]);
  });

  it("reports a theme choice and a style setting change, which mark no block", () => {
    const draft = doc(LIVE().blocks.sections, { theme: "none", settings: { ...DEFAULT_STYLE, font_family: "georgia" } });

    const changes = diffDocuments(LIVE(), draft);

    expect(changes).toEqual([
      { key: "theme", tag: "Changed", where: "Theme", what: "default → none" },
      { key: "settings", tag: "Changed", where: "Document style", what: "Font changed" },
    ]);
    expect(changes.every((change) => change.markId === undefined)).toBe(true);
  });

  it("compares code templates by their parts, and a template written another way as one change", () => {
    const code = (subject: string, html: string) => ({ schema: 1, channel: "email", mode: "code", email: { subject, html, text: "x" } });

    expect(diffDocuments(code("A", "<p>1</p>"), code("A", "<p>2</p>"))).toEqual([{ key: "source", tag: "Changed", where: "HTML", what: "The source changed" }]);
    expect(diffDocuments(code("A", "<p>1</p>"), code("B", "<p>1</p>"))).toEqual([{ key: "subject", tag: "Changed", where: "Subject", what: "A → B" }]);
    expect(diffDocuments(code("A", "<p>1</p>"), LIVE())).toEqual([{ key: "document", tag: "Changed", where: "Template", what: "Written as blocks now, was HTML" }]);
  });
});
