import { describe, expect, it } from "vitest";

import type { Graph } from "./model";
import { countsHeight, pathOverlay } from "./overlay";

/** A branch whose two arms lead to the same step, and a plain step with one wire to the next. */
const GRAPH = {
  v: 1,
  nodes: [
    { id: "b", type: "branch", conditions: [] },
    { id: "x", type: "wait.duration", seconds: 60 },
    { id: "end", type: "exit" },
  ],
  edges: [
    { id: "yes", from: "b", port: "case_0", to: "x" },
    { id: "no", from: "b", port: "else", to: "x" },
    { id: "next", from: "x", port: "next", to: "end" },
  ],
} as unknown as Graph;

describe("pathOverlay", () => {
  it("lights the wire of the port the path took", () => {
    const { pathEdges } = pathOverlay(GRAPH, [{ node: "b", port: "else" }, { node: "x", port: "next" }, { node: "end" }]);
    expect(pathEdges?.has("no")).toBe(true);
    expect(pathEdges?.has("yes")).toBe(false);
    expect(pathEdges?.has("next")).toBe(true);
  });

  it("lights none of several wires between two steps when the port is unknown, and keeps the numbers", () => {
    for (const port of [undefined, null]) {
      const { pathEdges, steps } = pathOverlay(GRAPH, [{ node: "b", port }, { node: "x" }, { node: "end" }]);
      expect(pathEdges?.has("yes")).toBe(false);
      expect(pathEdges?.has("no")).toBe(false);
      expect(steps?.get("b")).toEqual([1]);
      expect(steps?.get("x")).toEqual([2]);
      // A single wire between two steps is certain without a port.
      expect(pathEdges?.has("next")).toBe(true);
    }
  });
});

describe("count badge height", () => {
  it("adds one line for the short badges of a plain step and a line per skip reason", () => {
    expect(countsHeight({ entered: 1, waiting: 1, failed: 0 })).toBe(26);
    expect(countsHeight({ entered: 1, waiting: 1, failed: 1 })).toBe(52);
    expect(countsHeight({ entered: 1, waiting: 0, failed: 0, skipped: [{ reason: "a", count: 1 }] })).toBe(52);
  });
});
