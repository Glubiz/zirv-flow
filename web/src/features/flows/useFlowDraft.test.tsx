import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

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

  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  const flush = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));

  function mountDraft() {
    return renderHook(() =>
      useFlowDraft({ workspaceId: "ws1", flow: flowDetail() as unknown as FlowDetail, editable: true }),
    );
  }

  it("keeps the unsaved draft and its undo while held, takes no edit, and saves nothing", async () => {
    const calls = mockApi(flowRoutes());
    const { result } = mountDraft();
    act(() => result.current.edit((draft) => ({ ...draft, name: "Edited" })));

    await act(() => result.current.hold());
    expect(result.current.held).toBe(true);
    act(() => result.current.edit((draft) => ({ ...draft, name: "While held" })));
    await flush(1500);

    expect(result.current.draft.name).toBe("Edited");
    expect(result.current.unsaved).toBe(true);
    expect(result.current.canUndo).toBe(true);
    expect(saves(calls)).toHaveLength(0);
  });

  it("gives the draft back as it was when the hold is released, and saves what was waiting", async () => {
    const calls = mockApi(flowRoutes());
    const { result } = mountDraft();
    act(() => result.current.edit((draft) => ({ ...draft, name: "Edited" })));
    await act(() => result.current.hold());

    act(() => result.current.release());
    expect(result.current.held).toBe(false);
    expect(result.current.draft.name).toBe("Edited");
    await flush(1000);

    expect(saves(calls)).toHaveLength(1);
    expect(saves(calls)[0]).toMatchObject({ name: "Edited" });
    act(() => result.current.undo());
    expect(result.current.draft.name).toBe("Welcome series");
  });

  it("shows the adopted draft as saved, makes it one step of undo, and saves nothing", async () => {
    const calls = mockApi(flowRoutes());
    const { result } = mountDraft();
    act(() => result.current.edit((draft) => ({ ...draft, name: "Edited" })));
    await act(() => result.current.hold());

    act(() => result.current.adopt(replacement));
    await flush(1500);

    expect(result.current.draft.graph).toEqual(replacement.graph);
    expect(result.current.held).toBe(false);
    expect(result.current.unsaved).toBe(false);
    expect(saves(calls)).toHaveLength(0);
    act(() => result.current.edit((draft) => ({ ...draft, name: "After" })));
    await flush(1000);
    expect(saves(calls)).toHaveLength(1);
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
    const { result } = mountDraft();
    act(() => result.current.edit((draft) => ({ ...draft, name: "One" })));
    await flush(1000);
    expect(saves(calls)).toHaveLength(1);
    act(() => result.current.edit((draft) => ({ ...draft, name: "Two" })));

    let finished = false;
    const holding = result.current.hold().then(() => {
      finished = true;
    });
    await flush(50);
    expect(finished).toBe(false);
    await act(async () => {
      release();
      await holding;
    });
    await flush(1500);

    expect(saves(calls)).toHaveLength(1);
    expect(result.current.draft.name).toBe("Two");
  });
});
