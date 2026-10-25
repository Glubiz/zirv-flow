import { describe, expect, it } from "vitest";

import { applyDrop, canDrop, emptyContent, moveBlock, moveSection, newBlock, newSection, readContent, type BlocksContent } from "./document";
import { cleanRichText, linkAllowed } from "./richText";

function sample(): BlocksContent {
  const content = emptyContent("marketing");
  return {
    ...content,
    sections: [
      { id: "s1", columns: [[{ type: "divider", id: "a" }], [{ type: "divider", id: "b" }]] },
      { id: "s2", columns: [[{ type: "divider", id: "c" }]] },
    ],
  };
}

describe("the block document operations", () => {
  it("makes ids that never collide", () => {
    const content = sample();
    expect(newBlock("heading", content).id).toBe("heading-1");
    expect(newSection(2, content).id).toBe("section-1");
    expect(newSection(2, content).columns).toEqual([[], []]);
  });

  it("allows a section only at the root and a block only in a column", () => {
    const content = sample();
    expect(canDrop(content, { kind: "section", id: "s1" }, { kind: "column", section: 1, column: 0, index: 0 })).toBe(false);
    expect(canDrop(content, { kind: "new-section", columns: 2 }, { kind: "column", section: 0, column: 0, index: 0 })).toBe(false);
    expect(canDrop(content, { kind: "block", id: "a" }, { kind: "root", index: 0 })).toBe(false);
    expect(canDrop(content, { kind: "section", id: "s1" }, { kind: "root", index: 2 })).toBe(true);
    expect(applyDrop(content, { kind: "section", id: "s1" }, { kind: "column", section: 1, column: 0, index: 0 })).toBeNull();
  });

  it("moves a block across columns at the edge of a column", () => {
    const content = sample();
    const down = moveBlock(content, "a", "down");
    expect(down?.sections[0]?.columns.map((column) => column.map((block) => block.id))).toEqual([[], ["a", "b"]]);
    const up = moveBlock(content, "c", "up");
    expect(up?.sections[0]?.columns[1]?.map((block) => block.id)).toEqual(["b", "c"]);
    expect(moveBlock(content, "a", "up")).toBeNull();
  });

  it("moves sections and keeps their blocks", () => {
    const content = sample();
    expect(moveSection(content, "s1", "down")?.sections.map((section) => section.id)).toEqual(["s2", "s1"]);
    expect(moveSection(content, "s1", "up")).toBeNull();
  });

  it("reads a stored document without adding fields", () => {
    const read = readContent({ subject: "Hi", sections: [{ id: "s", columns: [[{ type: "divider", id: "d" }]] }] });
    expect(read.sections[0]?.columns[0]).toEqual([{ type: "divider", id: "d" }]);
    expect("sender" in read).toBe(false);
  });
});

describe("the rich text clean pass", () => {
  it("keeps only what the server allows", () => {
    const html = '<div class="x" style="color:red">Hi <span>there</span> <b onclick="x()">bold</b></div><script>bad()</script><h1>Title</h1><img src="x">';
    expect(cleanRichText(html)).toBe("<p>Hi there <b>bold</b></p><p>Title</p>");
  });

  it("keeps lists, line breaks and allowed links, and drops other links", () => {
    expect(cleanRichText('<ul><li>One</li><li>Two</li></ul><p>a<br>b <a href="https://a.example" target="_blank">ok</a> <a href="javascript:x()">no</a></p>')).toBe(
      '<ul><li>One</li><li>Two</li></ul><p>a<br>b <a href="https://a.example">ok</a> no</p>',
    );
  });

  it("leaves merge tags as written and escapes only the text around them", () => {
    expect(cleanRichText('<p>Hi {{ person.n | default("a & b") }} & more</p>')).toBe('<p>Hi {{ person.n | default("a & b") }} &amp; more</p>');
    expect(cleanRichText('<p><a href="{{ system.unsubscribe_url }}">Unsubscribe</a></p>')).toBe('<p><a href="{{ system.unsubscribe_url }}">Unsubscribe</a></p>');
  });

  it("wraps loose text in a paragraph, is stable, and gives nothing for nothing", () => {
    const once = cleanRichText("loose <i>text</i>");
    expect(once).toBe("<p>loose <i>text</i></p>");
    expect(cleanRichText(once)).toBe(once);
    expect(cleanRichText("")).toBe("");
    expect(cleanRichText("<p></p>")).toBe("");
  });

  it("accepts the schemes a link may use", () => {
    expect(["https://a", "http://a", "mailto:a@b", "{{ u }}"].every(linkAllowed)).toBe(true);
    expect(["javascript:a", "ftp://a", "/relative"].some(linkAllowed)).toBe(false);
  });
});
