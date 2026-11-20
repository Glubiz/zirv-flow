import { describe, expect, it } from "vitest";

import { chainToApi, newTransform, parseEntries, transformFromApi, transformToApi, type Transform } from "./transforms";

describe("transform drafts", () => {
  it("round trip through the API form for every step that has parameters", () => {
    const steps: Transform[] = [
      { op: "trim" },
      { op: "cast", to: "number" },
      { op: "timestamp", format: "%d/%m/%Y" },
      { op: "concat", columns: ["a.b"], separator: "-" },
      { op: "constant", value: "x" },
      { op: "value_map", map: { M: "male" } },
      { op: "split", separator: ";" },
    ];

    for (const step of steps) {
      expect(transformToApi(transformFromApi(step))).toEqual(step);
    }
  });

  it("are incomplete until the step has what it needs, and a chain leaves the incomplete ones out", () => {
    const join = newTransform("concat");
    const map = newTransform("value_map");
    const trim = newTransform("trim");

    expect(transformToApi(join)).toBeNull();
    expect(transformToApi(map)).toBeNull();
    expect(chainToApi([join, trim, map])).toEqual([{ op: "trim" }]);
  });

  it("read one old=new replacement per line", () => {
    expect(parseEntries("M=male\r\nF = female\nbroken\n=x")).toEqual({ M: "male", F: "female", "": "x" });
  });
});

describe("a value map in the text form", () => {
  const roundTrip = (map: Record<string, string>) => transformToApi(transformFromApi({ op: "value_map", map }));

  it("holds an empty key and a key with = without changing the map", () => {
    const stored: Transform = { op: "value_map", map: { "": "unknown", "a=b": "c" } };
    expect(transformToApi(transformFromApi(stored))).toEqual(stored);
  });

  it("reads \\= and \\\\ in a key, splitting at the first = that is not escaped", () => {
    expect(parseEntries("a\\=b=c\nd\\\\=e")).toEqual({ "a=b": "c", "d\\": "e" });
  });

  it("keeps a backslash that is not an escape, in a key or a value, as it was typed", () => {
    expect(parseEntries("C:\\dir=a\\b\nk=\\q\nend=x\\")).toEqual({ "C:\\dir": "a\\b", k: "\\q", end: "x\\" });
  });

  it("round-trips keys and values the plain text could not hold", () => {
    const map: Record<string, string> = {
      "a\\b": "x",
      "\\=": "y",
      "line\nbreak": "two\r\nlines",
      " padded ": "\tedge\u00a0",
      "   ": "blank key",
      k: "",
      "\\n": "literal backslash n",
      "\\": "\\",
      "trailing\\": "\\s",
      "u\u2028x": "\u2028",
    };
    expect(roundTrip(map)).toEqual({ op: "value_map", map });
  });

  it("round-trips a __proto__ key as an own property", () => {
    const map = Object.fromEntries([
      ["__proto__", "x"],
      ["a", "b"],
    ]);
    const got = (roundTrip(map) as Extract<Transform, { op: "value_map" }>).map;
    expect(Object.keys(got).sort()).toEqual(["__proto__", "a"]);
    expect(Object.getOwnPropertyDescriptor(got, "__proto__")?.value).toBe("x");
  });

  it("shows an ordinary map exactly as one line per pair", () => {
    expect(transformFromApi({ op: "value_map", map: { M: "male", "a b": "c d", "C:/x": "p/q" } }).entries).toBe("M=male\na b=c d\nC:/x=p/q");
  });

  it("shows a backslash that starts no escape as it is stored, and doubles one that would", () => {
    const map = { "C:\\dir": "a\\b", path: "C:\\dir\\", n: "\\n", u: "\\u0041", key: "x" };
    expect(transformFromApi({ op: "value_map", map }).entries).toBe("C:\\dir=a\\b\npath=C:\\dir\\\nn=\\\\n\nu=\\\\u0041\nkey=x");
    expect(transformFromApi({ op: "value_map", map: { "end\\": "v" } }).entries).toBe("end\\\\=v");
    expect(roundTrip(map)).toEqual({ op: "value_map", map });
  });

  it("is complete with only an empty key, and incomplete with no entry", () => {
    expect(roundTrip({ "": "unknown" })).toEqual({ op: "value_map", map: { "": "unknown" } });
    expect(transformToApi({ ...newTransform("value_map"), entries: "" })).toBeNull();
  });
});
