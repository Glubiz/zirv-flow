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

  it("draws nothing for no records", () => {
    const { container } = render(<TraceTree label="Trace" records={[]} />);

    expect(container).toBeEmptyDOMElement();
  });
});
