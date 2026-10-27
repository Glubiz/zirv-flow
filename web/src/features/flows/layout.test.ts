import { describe, expect, it } from "vitest";

import { sampleGraph } from "../../test/flowsApi";
import { layout } from "./layout";
import { addNode, durationText, removeIndexedPort, TRIGGER_ID, wire, type Graph } from "./model";

const graph = sampleGraph() as unknown as Graph;

describe("flow layout", () => {
  it("puts the trigger first and each step one row below what leads into it", () => {
    const plan = layout(graph);

    expect(plan.order).toEqual([TRIGGER_ID, "n1", "n2", "n3", "n4", "n5"]);
    expect(plan.rows).toEqual([[TRIGGER_ID], ["n1"], ["n2"], ["n3", "n4"], ["n5"]]);
    const depth = (id: string) => plan.placed.get(id)?.depth;
    expect([depth("n1"), depth("n2"), depth("n3"), depth("n4"), depth("n5")]).toEqual([1, 2, 3, 3, 4]);
  });

  it("orders siblings by port order and never overlaps them", () => {
    const plan = layout(graph);
    const yes = plan.placed.get("n3")!;
    const no = plan.placed.get("n4")!;

    expect(yes.x).toBeLessThan(no.x);
    expect(yes.x + yes.width).toBeLessThanOrEqual(no.x);
    expect(yes.y).toBe(no.y);
  });

  it("places a new, unwired step after the rest", () => {
    const { graph: grown, id } = addNode(graph, "wait.duration");
    const plan = layout(grown);

    expect(id).toBe("n6");
    expect(plan.placed.get(id)?.reachable).toBe(false);
    expect(plan.order.at(-1)).toBe(id);
  });

  it("lays out a graph with a loop without hanging", () => {
    const looped = wire(graph, "n5", "next", "n1");
    expect(() => layout(looped)).not.toThrow();
  });

  it("draws only the trigger for an empty flow", () => {
    expect(layout({ v: 1, nodes: [], edges: [] }).order).toEqual([TRIGGER_ID]);
  });
});

describe("flow graph edits", () => {
  it("moves the wires of the ports after a removed case up by one", () => {
    const branch = { id: "b", type: "branch" as const, conditions: [{ all: [] }, { all: [] }, { all: [] }] };
    const wired: Graph = {
      v: 1,
      nodes: [branch],
      edges: [
        { id: "e1", from: "b", port: "case_0", to: "x" },
        { id: "e2", from: "b", port: "case_1", to: "y" },
        { id: "e3", from: "b", port: "case_2", to: "z" },
        { id: "e4", from: "b", port: "else", to: "w" },
      ],
    };

    const next = removeIndexedPort(wired, { ...branch, conditions: branch.conditions.slice(0, 2) }, "case_", 1);

    expect(next.edges.map((edge) => [edge.port, edge.to])).toEqual([
      ["case_0", "x"],
      ["case_1", "z"],
      ["else", "w"],
    ]);
  });

  it("says a duration in the largest unit that fits", () => {
    expect([durationText(86_400), durationText(172_800), durationText(90), durationText(3_600), durationText(604_800)]).toEqual([
      "1 day",
      "2 days",
      "90 seconds",
      "1 hour",
      "1 week",
    ]);
  });
});

describe("flow layout with count badges", () => {
  it("grows the card of a counted step and moves the rows below it down by as much", () => {
    const plain = layout(graph);
    const grown = layout(graph, new Map([["n2", 30]]));

    expect(grown.placed.get("n2")?.height).toBe((plain.placed.get("n2")?.height ?? 0) + 30);
    expect(grown.placed.get("n3")?.y).toBe((plain.placed.get("n3")?.y ?? 0) + 30);
    expect(grown.placed.get("n1")).toEqual(plain.placed.get("n1"));
  });
});
