import { describe, expect, it } from "vitest";

import { nameProblem, parseQueries, queriesValid, serializeQueries, sqlProblem } from "./queries";

describe("queries", () => {
  it("round-trips through the text a form keeps, and an empty list is no text", () => {
    const rows = [{ name: "vip", sql: "SELECT 1" }];

    expect(parseQueries(serializeQueries(rows))).toEqual(rows);
    expect(serializeQueries([])).toBe("");
    expect(parseQueries("")).toEqual([]);
  });

  it("refuses text that is not a list of named statements", () => {
    for (const bad of ["nope", "{}", "[1]", '[{"name":"a"}]', '[{"name":1,"sql":"x"}]', "[null]"]) {
      expect(parseQueries(bad), bad).toBeNull();
    }
  });

  it("applies the server's rules to names", () => {
    const rows = (...names: string[]) => names.map((name) => ({ name, sql: "SELECT 1" }));

    expect(nameProblem(rows("ok_1"), 0)).toBeUndefined();
    expect(nameProblem(rows(""), 0)).toMatch(/Give the query a name/);
    expect(nameProblem(rows("Upper"), 0)).toMatch(/a to z/);
    expect(nameProblem(rows("has space"), 0)).toMatch(/a to z/);
    expect(nameProblem(rows("a".repeat(65)), 0)).toMatch(/a to z/);
    expect(nameProblem(rows("a".repeat(64)), 0)).toBeUndefined();
    expect(nameProblem(rows("a", "a"), 1)).toMatch(/Another query/);
  });

  it("applies the server's rules to SQL", () => {
    expect(sqlProblem("select 1")).toBeUndefined();
    expect(sqlProblem("  WITH a AS (SELECT 1) SELECT * FROM a")).toBeUndefined();
    expect(sqlProblem("SELECT(1)")).toBeUndefined();
    expect(sqlProblem("")).toMatch(/Enter/);
    expect(sqlProblem("SELECT 1; ")).toMatch(/semicolon/);
    expect(sqlProblem("SELECTED 1")).toMatch(/SELECT or WITH/);
    expect(sqlProblem(`SELECT '${"x".repeat(20_000)}'`)).toMatch(/under 20,000/);
  });

  it("is valid only when every row is, and at most twenty", () => {
    const good = (n: number) => Array.from({ length: n }, (_, at) => ({ name: `q${at}`, sql: "SELECT 1" }));

    expect(queriesValid("")).toBe(true);
    expect(queriesValid(serializeQueries(good(20)))).toBe(true);
    expect(queriesValid(serializeQueries(good(21)))).toBe(false);
    expect(queriesValid(serializeQueries([{ name: "", sql: "SELECT 1" }]))).toBe(false);
    expect(queriesValid("garbage")).toBe(false);
  });
});
