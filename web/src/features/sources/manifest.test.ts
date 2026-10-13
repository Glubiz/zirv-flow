import { describe, expect, it } from "vitest";

import { analyze, lineOfPointer, offsetOfLine, prettyPrint, STARTERS, starterManifest } from "./manifest";

const TEXT = `{
  "version": "v1",
  "streams": [
    {
      "name": "contacts",
      "fields": [
        { "name": "id" }
      ]
    }
  ],
  "a/b": { "c~d": 1 }
}`;

describe("reading a manifest as text", () => {
  it("returns the value and the line each pointer starts on", () => {
    const analysis = analyze(TEXT);

    expect(analysis.ok).toBe(true);
    if (!analysis.ok) return;
    expect((analysis.value as { version: string }).version).toBe("v1");
    expect(analysis.lines.get("")).toBe(1);
    expect(analysis.lines.get("/version")).toBe(2);
    expect(analysis.lines.get("/streams")).toBe(3);
    expect(analysis.lines.get("/streams/0/name")).toBe(5);
    expect(analysis.lines.get("/streams/0/fields/0/name")).toBe(7);
    expect(analysis.lines.get("/a~1b/c~0d")).toBe(11);
  });

  it("reports where the first syntax problem is and nothing of the text", () => {
    const text = '{\n  "token": "SECRET-value",\n  "x": ]\n}';

    const analysis = analyze(text);

    expect(analysis.ok).toBe(false);
    if (analysis.ok) return;
    expect(analysis.problem).toEqual({ line: 3, column: 8 });
    expect(JSON.stringify(analysis)).not.toContain("SECRET");
  });

  it("refuses what JSON.parse refuses", () => {
    for (const bad of ["", "{", '{"a":}', '{"a" 1}', "[1,]", "{'a':1}", '{"a":1} x', "tru", '"\u0001"', "{,}"]) {
      expect(analyze(bad).ok, JSON.stringify(bad)).toBe(false);
    }
  });

  it("accepts what JSON.parse accepts, whatever the whitespace", () => {
    for (const good of ["{}", "[]", " 1 ", '"x"', "null", '{"a":[1,2.5e3,-0,true,false,null,{"b":"\\u00e9\\n"}]}']) {
      expect(analyze(good).ok, good).toBe(true);
    }
  });

  it("stops on a manifest nested absurdly deep instead of overflowing", () => {
    const deep = "[".repeat(5000) + "]".repeat(5000);

    expect(analyze(deep).ok).toBe(false);
  });
});

describe("finding an issue in the text", () => {
  const analysis = analyze(TEXT);
  const lines = analysis.ok ? analysis.lines : new Map<string, number>();

  it("uses the line of the key, or of the nearest key that exists", () => {
    expect(lineOfPointer(lines, "/streams/0/name")).toBe(5);
    expect(lineOfPointer(lines, "/streams/0/missing")).toBe(4);
    expect(lineOfPointer(lines, "/nowhere/at/all")).toBe(1);
    expect(lineOfPointer(lines, "")).toBe(1);
  });

  it("gives the offset of the start of a line", () => {
    expect(offsetOfLine(TEXT, 1)).toBe(0);
    expect(TEXT.slice(offsetOfLine(TEXT, 2), offsetOfLine(TEXT, 2) + 11)).toBe('  "version"');
    expect(offsetOfLine(TEXT, 999)).toBe(TEXT.length);
  });
});

describe("pretty printing and starters", () => {
  it("indents valid JSON and leaves invalid text alone", () => {
    expect(prettyPrint('{"a":[1]}')).toBe('{\n  "a": [\n    1\n  ]\n}');
    expect(prettyPrint("{")).toBeNull();
  });

  it("offers a starter for every kind of authentication and each is valid JSON of manifest v1", () => {
    expect(STARTERS.map((starter) => starter.id)).toEqual([
      "none",
      "api_key",
      "basic",
      "bearer",
      "oauth2_client_credentials",
      "oauth2_authorization_code",
    ]);
    for (const starter of STARTERS) {
      const analysis = analyze(starterManifest(starter.id));
      expect(analysis.ok, starter.id).toBe(true);
      if (!analysis.ok) continue;
      const manifest = analysis.value as { version: string; auth: { type: string } };
      expect(manifest.version).toBe("v1");
      expect(manifest.auth.type).toBe(starter.id);
    }
  });
});
