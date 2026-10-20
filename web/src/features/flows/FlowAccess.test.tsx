import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { flowRoutes } from "../../test/flowsApi";
import { lastGraph, mountEditor as mount, openSheetOf, saves, type User } from "../../test/flowsEditor";
import { mockApi } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

beforeEach(() => {
  stubReactFlow();
});

const SAVE = { timeout: 3000 };

describe("flow editor for a reader", () => {
  it("offers no editing: no edit tools, no inputs to change, no saving", async () => {
    const calls = mockApi(flowRoutes({ role: "reader" }));
    const user = userEvent.setup();
    mount();

    await screen.findByRole("group", { name: /^Wait: 1 day/ });
    expect(screen.queryByRole("button", { name: "Add a step" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "Flow name" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Fit to screen" })).toBeInTheDocument();

    const sheet = await openSheetOf(user, /^Wait: 1 day/);
    expect(within(sheet).getByLabelText("Amount")).toBeDisabled();
    expect(within(sheet).getByLabelText("Next step")).toBeDisabled();
    expect(within(sheet).queryByRole("button", { name: "Delete step…" })).not.toBeInTheDocument();

    await user.click(screen.getByRole("tab", { name: "Flow settings" }));
    expect(await screen.findByLabelText("Starts when")).toBeDisabled();
    expect(calls.some((call) => call.method === "PATCH" || call.path.endsWith("/validate"))).toBe(false);
  });

  it("does not delete on the Delete key", async () => {
    mockApi(flowRoutes({ role: "reader" }));
    const user = userEvent.setup();
    mount();
    const step = await screen.findByRole("group", { name: /^Webhook: / });
    step.focus();

    await user.keyboard("{Delete}");

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
});

describe("flow editor keyboard", () => {
  async function tabToTrigger(user: User) {
    for (let press = 0; press < 30; press += 1) {
      await user.tab();
      if (document.activeElement?.getAttribute("aria-label")?.startsWith("Trigger:")) return;
    }
    throw new Error("Tab never reached the trigger");
  }

  it("reaches the nodes with Tab and follows the wires with the arrows", async () => {
    mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();
    await screen.findByRole("group", { name: /^Wait: 1 day/ });

    await tabToTrigger(user);
    await user.keyboard("{ArrowDown}");
    expect(screen.getByRole("group", { name: /^Wait: 1 day/ })).toHaveFocus();
    await user.keyboard("{ArrowDown}");
    expect(screen.getByRole("group", { name: /^Branch: / })).toHaveFocus();
    await user.keyboard("{ArrowDown}");
    expect(screen.getByRole("group", { name: /^Update a trait: / })).toHaveFocus();
    await user.keyboard("{ArrowRight}");
    expect(screen.getByRole("group", { name: /^Webhook: / })).toHaveFocus();
    await user.keyboard("{ArrowLeft}");
    expect(screen.getByRole("group", { name: /^Update a trait: / })).toHaveFocus();
    await user.keyboard("{ArrowUp}");
    expect(screen.getByRole("group", { name: /^Branch: / })).toHaveFocus();
  });

  it("opens the sheet with Enter, and Escape closes it and returns focus to the node", async () => {
    mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();
    const step = await screen.findByRole("group", { name: /^Wait: 1 day/ });
    step.focus();

    await user.keyboard("{Enter}");
    expect(await screen.findByRole("dialog")).toHaveFocus();

    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    await waitFor(() => expect(step).toHaveFocus());
  });

  it("asks before deleting a step, and sends nothing until it is confirmed", async () => {
    const calls = mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();
    const step = await screen.findByRole("group", { name: /^Webhook: / });
    step.focus();

    await user.keyboard("{Delete}");

    const dialog = await screen.findByRole("dialog", { name: "Delete this step?" });
    expect(saves(calls)).toHaveLength(0);
    expect(screen.getByRole("group", { name: /^Webhook: / })).toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "Delete step" }));

    await waitFor(() => expect(saves(calls)).toHaveLength(1), SAVE);
    const graph = lastGraph(calls);
    expect(graph?.nodes.some((node) => node.id === "n4")).toBe(false);
    expect(graph?.edges.some((edge) => edge.from === "n4" || edge.to === "n4")).toBe(false);
  });

  it("keeps the step when the deletion is cancelled", async () => {
    const calls = mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();
    const step = await screen.findByRole("group", { name: /^Webhook: / });
    step.focus();

    await user.keyboard("{Backspace}");
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Cancel" }));

    expect(screen.getByRole("group", { name: /^Webhook: / })).toBeInTheDocument();
    expect(saves(calls)).toHaveLength(0);
  });
});
