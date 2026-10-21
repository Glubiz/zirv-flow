import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { sampleGraph } from "../../test/flowsApi";
import { FlowChanges } from "./FlowChanges";
import type { DraftChange } from "../assistant";
import type { Graph } from "./model";

const before = sampleGraph() as unknown as Graph;
const names = { lists: [], segments: [] };

/** A change as the API reports it; the generated type cannot hold a scalar, which the server sends. */
function change(path: string, before: unknown, after: unknown): DraftChange {
  return { path, change: "changed", before, after } as DraftChange;
}

function changed(seconds: number): Graph {
  return { ...before, nodes: before.nodes.map((node) => (node.id === "n1" ? { ...node, seconds } : node)) };
}

describe("FlowChanges", () => {
  it("names a changed step by its before and after title, and lists settings changes", () => {
    render(
      <FlowChanges
        names={names}
        before={before}
        after={changed(172_800)}
        changes={[
          change("/graph/nodes/n1/seconds", 86_400, 172_800),
          change("/settings/reentry/mode", "once", "always"),
        ]}
      />,
    );

    const list = screen.getByRole("list", { name: "What the draft changes" });
    expect(list).toHaveTextContent(/Changed · Wait: 1 day → Wait: 2 days\s*n1/);
    expect(list).toHaveTextContent("Settings · reentry › mode: once → always");
  });

  it("keeps every raw field behind a disclosure", () => {
    render(
      <FlowChanges
        names={names}
        before={before}
        after={changed(172_800)}
        changes={[change("/graph/nodes/n1/seconds", 86_400, 172_800)]}
      />,
    );

    const disclosure = screen.getByText(/Show every field/).closest("details");
    expect(disclosure).not.toBeNull();
    expect(disclosure).not.toHaveAttribute("open");
    expect(within(disclosure as HTMLElement).getByRole("table")).toBeInTheDocument();
  });

  it("says so when nothing changed", () => {
    render(<FlowChanges names={names} before={before} after={before} changes={[]} />);

    expect(screen.getByText("Same as the saved draft.")).toBeInTheDocument();
  });
});
