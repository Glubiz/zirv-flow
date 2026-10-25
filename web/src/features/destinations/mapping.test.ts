import { describe, expect, it } from "vitest";

import { mappingFromRows, newRow, rowsFromMapping } from "./mapping";

describe("forwarding mapping", () => {
  it("reads every kind of source into editable rows", () => {
    const rows = rowsFromMapping({
      id: { from: "event.id" },
      total: { from: "event.properties", pointer: "/order/total" },
      shop: { value: "main" },
      count: { value: 3 },
      test: { value: true },
    });

    expect(rows.map((row) => [row.key, row.source, row.pointer, row.valueType, row.value])).toEqual([
      ["id", "event.id", "", "string", ""],
      ["total", "event.properties", "/order/total", "string", ""],
      ["shop", "value", "", "string", "main"],
      ["count", "value", "", "number", "3"],
      ["test", "value", "", "boolean", "true"],
    ]);
  });

  it("writes rows back as the shape the API takes", () => {
    const result = mappingFromRows([
      { ...newRow(), key: "id", source: "person.id" },
      { ...newRow(), key: "total", source: "event.properties", pointer: "/total" },
      { ...newRow(), key: "n", source: "value", valueType: "number", value: "4.5" },
      { ...newRow(), key: "ok", source: "value", valueType: "boolean", value: "true" },
    ]);

    expect(result).toEqual({
      ok: true,
      mapping: {
        id: { from: "person.id" },
        total: { from: "event.properties", pointer: "/total" },
        n: { value: 4.5 },
        ok: { value: true },
      },
    });
  });

  it("names the row that is wrong", () => {
    const result = mappingFromRows([
      { ...newRow(), key: "a" },
      { ...newRow(), key: "a" },
      { ...newRow(), key: "" },
      { ...newRow(), key: "p", source: "event.properties", pointer: "total" },
      { ...newRow(), key: "n", source: "value", valueType: "number", value: "x" },
    ]);

    expect(result.ok).toBe(false);
    expect(Object.keys(result.ok ? {} : result.errors)).toEqual(["1", "2", "3", "4"]);
  });

  it("keeps an entry it does not recognise as it is, so an unedited save sends it back unchanged", () => {
    const mapping = {
      id: { from: "event.id" },
      empty: { value: null },
      odd: { from: "event.something_new" },
      text: "plain",
      list: [1, 2],
      bad_pointer: { from: "event.properties", pointer: 7 },
    };

    const rows = rowsFromMapping(mapping);
    const result = mappingFromRows(rows);

    expect(rows.filter((row) => row.raw !== undefined).map((row) => row.key)).toEqual(["empty", "odd", "text", "list", "bad_pointer"]);
    expect(result).toEqual({ ok: true, mapping });
  });
});
