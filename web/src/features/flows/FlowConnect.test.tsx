import { act, screen, waitFor } from "@testing-library/react";
import type * as XYFlow from "@xyflow/react";
import type { FinalConnectionState, ReactFlowProps } from "@xyflow/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { flowRoutes } from "../../test/flowsApi";
import { lastGraph, mountEditor as mount } from "../../test/flowsEditor";
import { mockApi } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

const captured = vi.hoisted(() => ({ props: null as ReactFlowProps | null }));

// Wraps the real ReactFlow only to hand the test the props the canvas gives it, so the test can call
// onConnectEnd the way xyflow does when a drag from an output lets go over a step.
vi.mock("@xyflow/react", async (importActual) => {
  const actual = await importActual<typeof XYFlow>();
  const Real = actual.ReactFlow;
  return {
    ...actual,
    ReactFlow: (props: ReactFlowProps) => {
      captured.props = props;
      return <Real {...props} />;
    },
  };
});

beforeEach(() => {
  stubReactFlow();
  captured.props = null;
});

describe("letting go of a drag over a step", () => {
  it("wires the output to that step", async () => {
    const graph = { v: 1, nodes: [{ id: "n1", type: "wait.duration", seconds: 60 }, { id: "n5", type: "exit" }], edges: [] };
    const calls = mockApi(flowRoutes({ flow: { graph } }));
    mount();
    await screen.findByRole("group", { name: /^Wait: / });
    const target = document.querySelector<HTMLElement>('.react-flow__node[data-id="n5"]');
    expect(target).not.toBeNull();
    const inner = document.createElement("span");
    target!.appendChild(inner);
    document.elementFromPoint = () => inner;

    act(() => {
      captured.props!.onConnectEnd!(new MouseEvent("mouseup", { clientX: 10, clientY: 10 }), {
        isValid: false,
        fromHandle: { type: "source", id: "next" },
        fromNode: { id: "n1" },
      } as unknown as FinalConnectionState);
    });

    await waitFor(() => expect(lastGraph(calls)?.edges).toEqual([expect.objectContaining({ from: "n1", port: "next", to: "n5" })]), {
      timeout: 3000,
    });
  });
});
