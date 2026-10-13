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
    expect(parseEntries("M=male\r\nF = female\nbroken\n=x")).toEqual({ M: "male", F: "female" });
  });
});
