import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { TraceTree, type TraceTreeRecord } from "./index";

function record(id: string, outcome: string, cause: { kind: string; id: string }, reason?: string): TraceTreeRecord {
  return { id, recorded_at: "2026-10-20T09:00:00Z", action: "flow.enrollment", outcome, cause, reason, subject_ids: {} };
}

const START = { kind: "trait_change", id: "c1" };
const OPERATOR = { kind: "manual", id: "u1" };

describe("trace tree", () => {
  it("keeps the order of the records and opens a branch for each decision with another cause", () => {
    render(
      <TraceTree
        label="Trace"
        records={[
          record("1", "enrolled", START),
          record("2", "failed", START, "boom"),
          record("3", "retried", OPERATOR, "operator"),
          record("4", "failed", START, "boom"),
        ]}
      />,
    );

    const tree = screen.getByRole("list", { name: "Trace" });
    const rows = Array.from(tree.querySelectorAll("li.trace-record")).map((row) => row.firstElementChild?.nextElementSibling?.textContent);
    expect(rows).toEqual(["enrolled", "failed", "retried", "failed"]);
    const retried = Array.from(tree.querySelectorAll("li.trace-record"))[2] as HTMLElement;
    expect(within(retried).getByRole("list")).toHaveTextContent("failed · boom");
  });

  it("roots a truncated trace on no cause, so an operator's decision is never the root", () => {
    render(<TraceTree label="Trace" truncated records={[record("3", "retried", OPERATOR, "operator"), record("4", "failed", START, "boom")]} />);

    const tree = screen.getByRole("list", { name: "Trace" });
    expect(tree.firstElementChild).toHaveTextContent("Earlier decisions are not shown");
    // The first thing in the tree is the notice; the operator's record is a row below it, with its own cause named.
    expect(tree.firstElementChild?.firstElementChild).toHaveTextContent("Earlier decisions are not shown");
    expect(tree.firstElementChild?.firstElementChild).not.toHaveTextContent("An operator");
    const rows = Array.from(tree.querySelectorAll("li.trace-record"));
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveTextContent("An operator");
    expect(rows[1]).toHaveTextContent("A change of a field or segment");
  });

  it("draws nothing for no records", () => {
    const { container } = render(<TraceTree label="Trace" records={[]} />);

    expect(container).toBeEmptyDOMElement();
  });
});
