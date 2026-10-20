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
