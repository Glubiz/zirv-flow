import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { FLOW_ID, flowRoutes, WS } from "../../test/flowsApi";
import { lastGraph, mountEditor as mount, openInspectorOf, saves } from "../../test/flowsEditor";
import { json, mockApi, problem } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

beforeEach(() => {
  stubReactFlow();
});

const SAVE = { timeout: 3000 };

describe("editing a step", () => {
  it("changes a wait in the inspector and saves the new graph", async () => {
    const calls = mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();

    const sheet = await openInspectorOf(user, /^Wait: 1 day/);
    const amount = within(sheet).getByLabelText("Amount");
    await user.clear(amount);
    await user.type(amount, "2");

    await waitFor(() => expect(saves(calls)).toHaveLength(1), SAVE);
    expect(lastGraph(calls)?.nodes.find((node) => node.id === "n1")?.["seconds"]).toBe(172_800);
    expect(await screen.findByText("Saved")).toBeInTheDocument();
  });

  it("edits a condition with the segment builder into the branch", async () => {
    const calls = mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();

    const sheet = await openInspectorOf(user, /^Branch: /);
    const value = await within(sheet).findByLabelText("Value");
    await user.clear(value);
    await user.type(value, "Odense");

    await waitFor(() => {
      const branch = lastGraph(calls)?.nodes.find((node) => node.id === "n2");
      expect(branch?.["conditions"]).toEqual([{ trait: "city", op: "eq", value: "Odense" }]);
    }, SAVE);
  });

  it("adds a case, which adds an output to wire", async () => {
    const calls = mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();

    const sheet = await openInspectorOf(user, /^Branch: /);
    await user.click(await within(sheet).findByRole("button", { name: "Add a case" }));

    expect(within(sheet).getByLabelText("Case 1 leads to")).toBeInTheDocument();
    expect(within(sheet).getByLabelText("Case 2 leads to")).toBeInTheDocument();
    expect(within(sheet).getByLabelText("Else leads to")).toBeInTheDocument();
    await waitFor(() => expect(saves(calls)).toHaveLength(1), SAVE);
  });
});

describe("wiring and undo", () => {
  it("wires a port from the select in the inspector, replacing the earlier wire", async () => {
    const calls = mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();

    const sheet = await openInspectorOf(user, /^Update a trait: /);
    await user.selectOptions(within(sheet).getByLabelText("Next step"), "n4");

    await waitFor(() => expect(saves(calls)).toHaveLength(1), SAVE);
    const edges = lastGraph(calls)?.edges.filter((edge) => edge.from === "n3");
    expect(edges).toHaveLength(1);
    expect(edges?.[0]).toMatchObject({ from: "n3", port: "next", to: "n4" });
  });

  it("disconnects a port from the inspector", async () => {
    const calls = mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();

    const sheet = await openInspectorOf(user, /^Update a trait: /);
    await user.click(within(sheet).getByRole("button", { name: "Disconnect Next" }));

    await waitFor(() => expect(saves(calls)).toHaveLength(1), SAVE);
    expect(lastGraph(calls)?.edges.some((edge) => edge.from === "n3")).toBe(false);
  });

  it("undoes the last edit and saves the restored graph", async () => {
    const calls = mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();

    const sheet = await openInspectorOf(user, /^Update a trait: /);
    await user.selectOptions(within(sheet).getByLabelText("Next step"), "n4");
    await waitFor(() => expect(saves(calls)).toHaveLength(1), SAVE);

    await user.click(screen.getByRole("button", { name: "Undo" }));

    await waitFor(() => expect(saves(calls)).toHaveLength(2), SAVE);
    expect(lastGraph(calls)?.edges.filter((edge) => edge.from === "n3")).toEqual([
      { id: "e4", from: "n3", port: "next", to: "n5" },
    ]);
    expect(screen.getByRole("button", { name: "Undo" })).toBeDisabled();
  });

  it("adds a step that starts unwired and opens its inspector", async () => {
    const calls = mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();
    await screen.findByRole("group", { name: /^Wait: 1 day/ });

    await user.click(screen.getByRole("button", { name: "Webhook" }));

    expect(await screen.findByRole("region", { name: "Step inspector" })).toBeInTheDocument();
    await waitFor(() => expect(saves(calls)).toHaveLength(1), SAVE);
    const graph = lastGraph(calls);
    expect(graph?.nodes.at(-1)).toMatchObject({ id: "n6", type: "action.webhook" });
    expect(graph?.edges.some((edge) => edge.from === "n6" || edge.to === "n6")).toBe(false);
  });

  it("shows a failed save with a retry that saves again", async () => {
    let attempts = 0;
    const routes = flowRoutes();
    const save = routes[`PATCH ${WS}/flows/${FLOW_ID}`]!;
    const calls = mockApi({
      ...routes,
      [`PATCH ${WS}/flows/${FLOW_ID}`]: (request) => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : save(request);
      },
    });
    const user = userEvent.setup();
    mount();

    const sheet = await openInspectorOf(user, /^Wait: 1 day/);
    await user.selectOptions(within(sheet).getByLabelText("Unit"), "hour");
    expect(await screen.findByText(/Couldn.t save/, undefined, SAVE)).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Retry" }));

    expect(await screen.findByText("Saved")).toBeInTheDocument();
    expect(saves(calls)).toHaveLength(2);
  });
});

describe("flow settings", () => {
  it("edits the trigger and saves the settings", async () => {
    const calls = mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("group", { name: /^Trigger:/ }));

    await user.selectOptions(await screen.findByLabelText("When a person"), "exited");

    await waitFor(() => expect(saves(calls)).toHaveLength(1), SAVE);
    expect((saves(calls)[0] as { settings: { trigger: unknown } }).settings.trigger).toMatchObject({
      type: "segment",
      on: "exited",
    });
  });

  it("moves between the views with the arrow keys", async () => {
    mockApi({ ...flowRoutes(), [`GET ${WS}/flows/${FLOW_ID}/versions`]: () => json([]) });
    const user = userEvent.setup();
    mount();
    const canvas = await screen.findByRole("link", { name: "Build" });
    canvas.focus();

    await user.keyboard("{ArrowRight}");
    expect(screen.getByRole("link", { name: "Test" })).toHaveFocus();
    expect(screen.getByRole("link", { name: "Test" })).toHaveAttribute("aria-current", "page");
    await user.keyboard("{ArrowRight}");
    expect(screen.getByRole("link", { name: "Results" })).toHaveFocus();
    expect(screen.getByRole("link", { name: "Results" })).toHaveAttribute("aria-current", "page");
    expect(await screen.findByText("No one can be in this flow yet")).toBeInTheDocument();
  });
});
