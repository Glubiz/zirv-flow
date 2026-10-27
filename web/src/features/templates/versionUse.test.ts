import { describe, expect, it } from "vitest";

import { DEFAULT_STYLE } from "./blocks/document";
import { snippetIds, themeChoice } from "./versionUse";

function blocks(theme: unknown, ...ids: string[]) {
  return {
    schema: 2,
    channel: "email",
    mode: "blocks",
    blocks: {
      subject: "Hello",
      preheader: "",
      category: "marketing",
      ...(theme === undefined ? {} : { theme }),
      footer: "theme",
      settings: { ...DEFAULT_STYLE },
      sections: [{ id: "s1", columns: [ids.map((id, index) => ({ type: "snippet", id: `b${index}`, snippet_id: id }))] }],
    },
  };
}

describe("themeChoice", () => {
  it("reads default, none and a named theme", () => {
    expect(themeChoice(blocks("default"))).toEqual({ kind: "default" });
    expect(themeChoice(blocks(undefined))).toEqual({ kind: "default" });
    expect(themeChoice(blocks("none"))).toEqual({ kind: "none" });
    expect(themeChoice(blocks({ id: "t1" }))).toEqual({ kind: "named", id: "t1" });
  });

  it("gives a code document no theme", () => {
    expect(themeChoice({ schema: 2, channel: "email", mode: "code", email: { subject: "x", html: "<p>x</p>" } })).toEqual({ kind: "none" });
  });
});

describe("snippetIds", () => {
  it("lists each named snippet once, in order", () => {
    expect(snippetIds(blocks("default", "a", "b", "a"))).toEqual(["a", "b"]);
  });

  it("finds none in a code document", () => {
    expect(snippetIds({ schema: 2, channel: "email", mode: "code", email: { subject: "x", html: "<p>x</p>" } })).toEqual([]);
  });
});
