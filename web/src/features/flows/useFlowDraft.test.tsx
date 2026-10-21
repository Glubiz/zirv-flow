import { act, renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { FLOW_ID, flowDetail, flowRoutes, WS } from "../../test/flowsApi";
import { json, mockApi, problem } from "../../test/mockApi";
import { saves } from "../../test/flowsEditor";
import type { FlowDetail } from "./model";
import { useFlowDraft } from "./useFlowDraft";

function pause(ms: number) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

describe("useFlowDraft", () => {
  it("flushes an edit made during a save that then fails, once no autosave timer is pending", async () => {
    const routes = flowRoutes();
    const save = routes[`PATCH ${WS}/flows/${FLOW_ID}`]!;
    let release: () => void = () => undefined;
    const held = new Promise<void>((resolve) => {
      release = resolve;
    });
    let patches = 0;
    const calls = mockApi({
      ...routes,
      [`PATCH ${WS}/flows/${FLOW_ID}`]: async (request) => {
        patches += 1;
        if (patches === 1) {
          await held;
          return problem(500, "Internal Server Error", "the first save failed");
        }
        return save(request);
      },
    });
    const { result } = renderHook(() =>
      useFlowDraft({ workspaceId: "ws1", flow: flowDetail() as unknown as FlowDetail, editable: true }),
    );

    act(() => result.current.edit((draft) => ({ ...draft, name: "One" })));
    await waitFor(() => expect(patches).toBe(1), { timeout: 3000 });
    act(() => result.current.edit((draft) => ({ ...draft, name: "Two" })));
    // The timer of the second edit fires while the first save is still in flight and finds it busy.
    await act(() => pause(1200));
    expect(patches).toBe(1);

    await act(async () => {
      release();
      await pause(0);
    });

    await waitFor(() => expect(saves(calls)).toHaveLength(2), { timeout: 3000 });
    expect(saves(calls)[1]).toMatchObject({ name: "Two" });
    await waitFor(() => expect(result.current.status).toBe("saved"));
    expect(result.current.unsaved).toBe(false);
  });

  it("leaves a failed save failed when nothing newer was edited", async () => {
    const routes = flowRoutes();
    let patches = 0;
    mockApi({
      ...routes,
      [`PATCH ${WS}/flows/${FLOW_ID}`]: () => {
        patches += 1;
        return problem(500, "Internal Server Error", "the save failed");
      },
      [`POST ${WS}/flows/${FLOW_ID}/validate`]: () => json({ valid: true, issues: [] }),
    });
    const { result } = renderHook(() =>
      useFlowDraft({ workspaceId: "ws1", flow: flowDetail() as unknown as FlowDetail, editable: true }),
    );

    act(() => result.current.edit((draft) => ({ ...draft, name: "One" })));
    await waitFor(() => expect(result.current.status).toBe("error"), { timeout: 3000 });
    await act(() => pause(1200));

    expect(patches).toBe(1);
  });

  it("reports nothing to save as saved after a failed save is undone back to the saved state", async () => {
    mockApi({
      ...flowRoutes(),
      [`PATCH ${WS}/flows/${FLOW_ID}`]: () => problem(500, "Internal Server Error", "the save failed"),
      [`POST ${WS}/flows/${FLOW_ID}/validate`]: () => json({ valid: true, issues: [] }),
    });
    const { result } = renderHook(() =>
      useFlowDraft({ workspaceId: "ws1", flow: flowDetail() as unknown as FlowDetail, editable: true }),
    );
    act(() => result.current.edit((draft) => ({ ...draft, name: "One" })));
    await waitFor(() => expect(result.current.status).toBe("error"), { timeout: 3000 });
    act(() => result.current.undo());

    let outcome = false;
    await act(async () => {
      outcome = await result.current.saveNow();
    });

    expect(outcome).toBe(true);
    expect(result.current.unsaved).toBe(false);
  });
});

describe("useFlowDraft replacing the draft", () => {
  const replacement = {
    graph: { v: 1, nodes: [{ id: "only", type: "exit" }], edges: [] },
    settings: { trigger: { type: "manual" }, reentry: { mode: "once" } },
  } as unknown as Pick<FlowDetail, "graph" | "settings">;

  it("drops the waiting save and the unsaved edit, takes no edit while held, and shows the adopted draft as saved", async () => {
    const calls = mockApi(flowRoutes());
    const { result } = renderHook(() =>
      useFlowDraft({ workspaceId: "ws1", flow: flowDetail() as unknown as FlowDetail, editable: true }),
    );
    act(() => result.current.edit((draft) => ({ ...draft, name: "Edited" })));
    expect(result.current.unsaved).toBe(true);

    await act(() => result.current.hold());
    expect(result.current.unsaved).toBe(false);
    expect(result.current.draft.name).toBe("Welcome series");
    act(() => result.current.edit((draft) => ({ ...draft, name: "While held" })));
    expect(result.current.draft.name).toBe("Welcome series");

    act(() => result.current.adopt(replacement));
    await act(() => pause(1200));

    expect(result.current.draft.graph).toEqual(replacement.graph);
    expect(result.current.unsaved).toBe(false);
    expect(saves(calls)).toHaveLength(0);
    // Edits are taken again once the draft is adopted.
    act(() => result.current.edit((draft) => ({ ...draft, name: "After" })));
    expect(result.current.draft.name).toBe("After");
    await waitFor(() => expect(saves(calls)).toHaveLength(1), { timeout: 3000 });
  });

  it("waits for a save that is on its way and does not start another after it", async () => {
    const routes = flowRoutes();
    const save = routes[`PATCH ${WS}/flows/${FLOW_ID}`]!;
    let release: () => void = () => undefined;
    const held = new Promise<void>((resolve) => {
      release = resolve;
    });
    const calls = mockApi({
      ...routes,
      [`PATCH ${WS}/flows/${FLOW_ID}`]: async (request) => {
        await held;
        return save(request);
      },
    });
    const { result } = renderHook(() =>
      useFlowDraft({ workspaceId: "ws1", flow: flowDetail() as unknown as FlowDetail, editable: true }),
    );
    act(() => result.current.edit((draft) => ({ ...draft, name: "One" })));
    await waitFor(() => expect(saves(calls)).toHaveLength(1), { timeout: 3000 });
    act(() => result.current.edit((draft) => ({ ...draft, name: "Two" })));

    let finished = false;
    const holding = result.current.hold().then(() => {
      finished = true;
    });
    await act(() => pause(50));
    expect(finished).toBe(false);
    await act(async () => {
      release();
      await holding;
    });
    await act(() => pause(1200));

    expect(saves(calls)).toHaveLength(1);
    expect(result.current.unsaved).toBe(false);
    expect(result.current.draft.name).toBe("One");
  });

  it("takes edits again after a hold is released", async () => {
    mockApi(flowRoutes());
    const { result } = renderHook(() =>
      useFlowDraft({ workspaceId: "ws1", flow: flowDetail() as unknown as FlowDetail, editable: true }),
    );
    await act(() => result.current.hold());
    act(() => result.current.release());
    act(() => result.current.edit((draft) => ({ ...draft, name: "Back" })));

    expect(result.current.draft.name).toBe("Back");
  });
});
