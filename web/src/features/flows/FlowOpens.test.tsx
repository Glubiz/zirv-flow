import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { flowRoutes } from "../../test/flowsApi";
import { lastGraph, mountEditor as mount, openInspectorOf, saves } from "../../test/flowsEditor";
import { mockApi } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

beforeEach(() => {
  stubReactFlow();
});

const SAVE = { timeout: 3000 };

const WAIT_GRAPH = {
  v: 1,
  nodes: [
    { id: "n1", type: "wait.until_event", event: "purchase", timeout_seconds: 3600 },
    { id: "n2", type: "exit" },
  ],
  edges: [
    { id: "e1", from: "n1", port: "event", to: "n2" },
    { id: "e2", from: "n1", port: "timeout", to: "n2" },
  ],
};

describe("the machine filter on opens", () => {
  it("is added, explained and removable in the wait editor", async () => {
    const calls = mockApi(flowRoutes({ flow: { graph: WAIT_GRAPH } }));
    const user = userEvent.setup();
    mount();

    const sheet = await openInspectorOf(user, /^Wait/);
    await user.selectOptions(within(sheet).getByLabelText("Event"), "email.opened");

    const filter = await within(sheet).findByRole("group", { name: "Filter 1" });
    expect(within(filter).getByLabelText("Property")).toHaveValue("machine");
    expect(within(filter).getByLabelText("Value")).toHaveValue("false");
    expect(within(sheet).getByText(/not detected as machine-generated/)).toBeInTheDocument();
    await waitFor(() => expect(saves(calls)).toHaveLength(1), SAVE);
    expect(lastGraph(calls)?.nodes[0]).toMatchObject({
      event: "email.opened",
      filters: [{ prop: "machine", op: "eq", value: false }],
    });

    await user.click(within(filter).getByRole("button", { name: "Remove filter 1" }));
    expect(within(sheet).queryByRole("group", { name: "Filter 1" })).not.toBeInTheDocument();
    await waitFor(() => expect(saves(calls)).toHaveLength(2), SAVE);
    expect(lastGraph(calls)?.nodes[0]).toMatchObject({ event: "email.opened", filters: [] });
  });

  it("is added to the goal event of the flow settings", async () => {
    const settings = {
      trigger: { type: "segment", segment_id: "22222222-0000-4000-8000-000000000001", on: "entered" },
      reentry: { mode: "once" },
      goal: { type: "event", event: "purchase", filters: [] },
    };
    const calls = mockApi(flowRoutes({ flow: { settings } }));
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("group", { name: /^Trigger:/ }));

    await user.selectOptions(await screen.findByLabelText("Goal event"), "email.opened");

    const filter = await screen.findByRole("group", { name: "Filter 1" });
    expect(within(filter).getByLabelText("Property")).toHaveValue("machine");
    expect(screen.getByText(/not detected as machine-generated/)).toBeInTheDocument();
    await waitFor(() => expect(saves(calls)).toHaveLength(1), SAVE);
    expect((saves(calls)[0] as { settings: { goal: unknown } }).settings.goal).toMatchObject({
      type: "event",
      event: "email.opened",
      filters: [{ prop: "machine", op: "eq", value: false }],
    });
  });
});
