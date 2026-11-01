import { describe, expect, it } from "vitest";

import type { Node } from "./ast";
import { namesOf } from "./describe";
import { builtFrom, describeDefinition, usedBy } from "./overview";

describe("describeDefinition", () => {
  it("joins the conditions of a group into one sentence", () => {
    const root = { all: [{ trait: "city", op: "eq", value: "Aarhus" }, { trait: "age", op: "gt", value: 30 }] } as unknown as Node;

    const text = describeDefinition(root, namesOf());

    expect(text.startsWith("People who match all of these: ")).toBe(true);
    expect(text).toContain("city");
    expect(text).toContain("age");
    expect(text.endsWith(".")).toBe(true);
  });

  it("says everyone for an empty group", () => {
    expect(describeDefinition({ all: [] }, namesOf())).toBe("Everyone.");
  });
});

describe("builtFrom", () => {
  it("names the lists and segments by their names and links them", () => {
    const [group] = builtFrom(
      { traits: ["city"], lists: ["l1"], segments: ["s1", "self"], consent: [["email", null]] },
      "ws1",
      [{ id: "l1", name: "Newsletter" }],
      [{ id: "s1", name: "Lapsed" }],
      "self",
    );

    expect(group?.items.map((item) => [item.label, item.meta, item.to])).toEqual([
      ["city", "field", undefined],
      ["Newsletter", "list", "/w/ws1/lists/l1"],
      ["Lapsed", "segment", "/w/ws1/segments/s1"],
      ["email", "consent", undefined],
    ]);
  });

  it("yields no items when the version records no dependencies", () => {
    expect(builtFrom({}, "ws1", [], [], "self")[0]?.items).toEqual([]);
  });
});

describe("usedBy", () => {
  it("names flows with Live or Draft and segments, and leaves empty groups empty", () => {
    const groups = usedBy(
      { flows: [{ flow_id: "f1", name: "Welcome", stage: "live" }, { flow_id: "f2", name: "Winback", stage: "draft" }], segments: [] },
      "ws1",
    );

    expect(groups[0]?.items.map((item) => [item.label, item.meta, item.to])).toEqual([
      ["Welcome", "Live", "/w/ws1/flows/f1"],
      ["Winback", "Draft", "/w/ws1/flows/f2"],
    ]);
    expect(groups[1]?.items).toEqual([]);
    expect(usedBy(null, "ws1").every((group) => group.items.length === 0)).toBe(true);
  });
});
