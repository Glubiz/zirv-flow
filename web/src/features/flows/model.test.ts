import { describe, expect, it } from "vitest";

import { wordsFor } from "./describe";
import { issueText } from "./issues";
import { connectionWire, type FlowIssue, type Graph } from "./model";

describe("connectionWire", () => {
  const connection = { source: "n1", sourceHandle: "case_0", target: "n2", targetHandle: null };

  it("turns a drag from an output to a step into a wire of that output", () => {
    expect(connectionWire(connection, true)).toEqual({ from: "n1", port: "case_0", to: "n2" });
  });

  it("makes no wire for a reader", () => {
    expect(connectionWire(connection, false)).toBeNull();
  });

  it("makes no wire from the trigger, from nowhere in particular, or from a step to itself", () => {
    expect(connectionWire({ ...connection, source: "__trigger" }, true)).toBeNull();
    expect(connectionWire({ ...connection, sourceHandle: null }, true)).toBeNull();
    expect(connectionWire({ ...connection, target: "n1" }, true)).toBeNull();
  });
});

describe("words for a new step", () => {
  it("titles a branch with no condition filled in as a prompt, not as 'is'", () => {
    const branch = { id: "n1", type: "branch" as const, conditions: [{ trait: "", op: "eq" as const }] };

    expect(wordsFor(branch).title).toBe("Choose a condition");
  });

  it("titles a branch on an empty event name as a prompt too", () => {
    const branch = { id: "n1", type: "branch" as const, conditions: [{ event: "" }] };

    expect(wordsFor(branch).title).toBe("Choose a condition");
  });
});

describe("issue text", () => {
  it("says an empty flow has no steps", () => {
    const empty: Graph = { v: 1, nodes: [], edges: [] };
    const issue: FlowIssue = { code: "no_entry" };

    expect(issueText(issue, empty)).toBe("The flow has no steps yet. Add the first step after the trigger.");
  });
});
