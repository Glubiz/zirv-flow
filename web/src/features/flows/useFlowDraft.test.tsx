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
    act(() => result.current.edit((draft) => ({ ...draft, settings: replacement.settings, graph: { ...draft.graph, edges: [] } })));
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

  it("saves nothing while held: saveNow answers false and retry does nothing", async () => {
    const calls = mockApi(flowRoutes());
    const { result } = mountDraft();
    act(() => result.current.edit((draft) => ({ ...draft, name: "Edited" })));
    await act(() => result.current.hold());

    let saved = true;
    await act(async () => {
      saved = await result.current.saveNow();
    });
    act(() => result.current.retry());
    await flush(1500);

    expect(saved).toBe(false);
    expect(saves(calls)).toHaveLength(0);
  });

  it("keeps a rename the person made when the accepted draft is adopted, and saves it", async () => {
    const calls = mockApi(flowRoutes());
    const { result } = mountDraft();
    act(() => result.current.edit((draft) => ({ ...draft, name: "Renamed" })));
    await act(() => result.current.hold());

    act(() => result.current.adopt(replacement));

    expect(result.current.draft.name).toBe("Renamed");
    expect(result.current.draft.graph).toEqual(replacement.graph);
    expect(result.current.unsaved).toBe(true);
    await flush(1000);
    expect(saves(calls)).toEqual([{ name: "Renamed" }]);
  });

  it("schedules nothing when the hold is released after the editor is gone", async () => {
    const calls = mockApi(flowRoutes());
    const { result, unmount } = mountDraft();
    act(() => result.current.edit((draft) => ({ ...draft, name: "Edited" })));
    await act(() => result.current.hold());
    const { release } = result.current;

    unmount();
    release();
    await flush(2000);

    expect(saves(calls)).toHaveLength(0);
  });

  describe("reconciling an accept that failed", () => {
    const FLOW = `${WS}/flows/${FLOW_ID}`;

    it("releases the hold and keeps the edits when the server's draft is unchanged", async () => {
      const calls = mockApi(flowRoutes());
      const { result } = mountDraft();
      act(() => result.current.edit((draft) => ({ ...draft, name: "Edited" })));
      await act(() => result.current.hold());

      let outcome = "";
      await act(async () => {
        outcome = await result.current.reconcile();
      });
      await flush(1000);

      expect(outcome).toBe("unchanged");
      expect(result.current.held).toBe(false);
      expect(result.current.draft.name).toBe("Edited");
      expect(saves(calls)).toEqual([{ name: "Edited" }]);
    });

    it("adopts the server's draft when it changed, and never saves the old edits over it", async () => {
      const routes = flowRoutes();
      const calls = mockApi({
        ...routes,
        [`GET ${FLOW}`]: () => json(flowDetail({ graph: replacement.graph, settings: replacement.settings })),
      });
      const { result } = mountDraft();
      act(() => result.current.edit((draft) => ({ ...draft, settings: { ...draft.settings, reentry: { mode: "always" } } })));
      await act(() => result.current.hold());

      let outcome = "";
      await act(async () => {
        outcome = await result.current.reconcile();
      });
      await flush(2000);

      expect(outcome).toBe("adopted");
      expect(result.current.draft.graph).toEqual(replacement.graph);
      expect(result.current.draft.settings).toEqual(replacement.settings);
      expect(result.current.unsaved).toBe(false);
      expect(saves(calls)).toHaveLength(0);
    });

    it("saves nothing until a retry when the flow cannot be read either", async () => {
      const calls = mockApi({
        ...flowRoutes(),
        [`GET ${FLOW}`]: () => problem(503, "Service Unavailable", "down"),
      });
      const { result } = mountDraft();
      act(() => result.current.edit((draft) => ({ ...draft, name: "Edited" })));
      await act(() => result.current.hold());

      let outcome = "";
      await act(async () => {
        outcome = await result.current.reconcile();
      });
      await flush(2000);

      expect(outcome).toBe("unknown");
      expect(result.current.held).toBe(false);
      expect(result.current.status).toBe("error");
      expect(result.current.unsaved).toBe(true);
      expect(saves(calls)).toHaveLength(0);

      act(() => result.current.retry());
      await flush(100);
      expect(saves(calls)).toEqual([{ name: "Edited" }]);
    });
  });
});
