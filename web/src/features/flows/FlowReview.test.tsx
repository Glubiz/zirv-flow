import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { FLOW_ID, flowRoutes, sampleGraph, WS } from "../../test/flowsApi";
import { lastGraph, mountEditor as mount, openSheetOf, saves } from "../../test/flowsEditor";
import { json, mockApi, problem, type Call, type Handler } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";
import { PURCHASE_DETAIL } from "../../test/segmentsApi";

beforeEach(() => {
  stubReactFlow();
});

const SAVE = { timeout: 3000 };

function graphOf(nodes: Record<string, unknown>[], edges: Record<string, unknown>[] = []) {
  return { v: 1, nodes, edges };
}

/** A promise that is settled from outside, for a request the test wants to hold. */
function gate<T>() {
  let release: (value: T) => void = () => undefined;
  const promise = new Promise<T>((resolve) => {
    release = resolve;
  });
  return { promise, release };
}

function pause(ms: number) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

describe("the add a step menu", () => {
  it("opens beside the rail, on screen, instead of ending at the button", async () => {
    mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();
    await screen.findByRole("group", { name: /^Wait: 1 day/ });

    await user.click(screen.getByRole("button", { name: "Add a step" }));

    expect(screen.getByRole("menu", { name: "Add a step" })).toHaveClass("menu-beside");
  });
});

describe("the issues popover", () => {
  const ISSUES = [{ code: "invalid_url", node: "n4", at: "/url" }];

  it("closes on Escape and returns focus to its button", async () => {
    mockApi(flowRoutes({ issues: ISSUES }));
    const user = userEvent.setup();
    mount();
    const button = await screen.findByRole("button", { name: "1 issue" });
    await user.click(button);
    expect(screen.getByRole("list", { name: "Issues in this flow" })).toBeInTheDocument();

    await user.keyboard("{Escape}");

    expect(screen.queryByRole("list", { name: "Issues in this flow" })).not.toBeInTheDocument();
    expect(button).toHaveFocus();
  });

  it("closes on a click outside it", async () => {
    mockApi(flowRoutes({ issues: ISSUES }));
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("button", { name: "1 issue" }));

    await user.click(document.body);

    expect(screen.queryByRole("list", { name: "Issues in this flow" })).not.toBeInTheDocument();
  });

  it("closes when the add a step menu opens", async () => {
    mockApi(flowRoutes({ issues: ISSUES }));
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("button", { name: "1 issue" }));

    await user.click(screen.getByRole("button", { name: "Add a step" }));

    expect(screen.queryByRole("list", { name: "Issues in this flow" })).not.toBeInTheDocument();
    expect(screen.getByRole("menu", { name: "Add a step" })).toBeInTheDocument();
  });
});

describe("issue words", () => {
  it("says an empty flow has no steps, rather than blaming a loop", async () => {
    mockApi(flowRoutes({ flow: { graph: graphOf([]) }, issues: [{ code: "no_entry" }] }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "1 issue" }));

    expect(screen.getByText("The flow has no steps yet. Add the first step after the trigger.")).toBeInTheDocument();
    expect(screen.queryByText(/there is a loop/)).not.toBeInTheDocument();
  });

  it("names the output of an unwired port in the list, as the sheet does", async () => {
    mockApi(flowRoutes({ issues: [{ code: "port_not_wired", node: "n2", at: "/ports/case_0" }] }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "1 issue" }));

    expect(screen.getByText("The Yes output is not connected to a next step.")).toBeInTheDocument();
  });

  it("shows an issue about the whole flow as text, not as a button that does nothing", async () => {
    mockApi(flowRoutes({ issues: [{ code: "too_many_nodes" }] }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "1 issue" }));

    const list = screen.getByRole("list", { name: "Issues in this flow" });
    expect(within(list).getByText("The flow has more steps than a flow may have.")).toBeInTheDocument();
    expect(within(list).queryByRole("button")).not.toBeInTheDocument();
  });
});

describe("a sheet per step", () => {
  const waits = graphOf(
    [
      { id: "n1", type: "wait.duration", seconds: 172_800 },
      { id: "n2", type: "wait.duration", seconds: 90 },
      { id: "n3", type: "exit" },
    ],
    [
      { id: "e1", from: "n1", port: "next", to: "n2" },
      { id: "e2", from: "n2", port: "next", to: "n3" },
    ],
  );

  it("opens another step fresh: its own unit, and focus inside it", async () => {
    mockApi(flowRoutes({ flow: { graph: waits } }));
    const user = userEvent.setup();
    mount();

    const first = await openSheetOf(user, /^Wait: 2 days/);
    expect(within(first).getByLabelText("Unit")).toHaveValue("day");

    await user.click(await screen.findByRole("group", { name: /^Wait: 90 seconds/ }));

    const second = await screen.findByRole("dialog");
    await waitFor(() => expect(second).toHaveFocus());
    expect(within(second).getByLabelText("Unit")).toHaveValue("second");
    expect(within(second).getByLabelText("Amount")).toHaveValue(90);
  });
});

describe("arm weights", () => {
  const split = graphOf([{ id: "n1", type: "split.random", arms: [{ weight: 50 }, { weight: 50 }] }]);

  function weights(calls: Call[]): number[][] {
    return saves(calls).flatMap((body) => {
      const node = (body as { graph?: { nodes: { arms?: { weight: number }[] }[] } }).graph?.nodes[0];
      return node?.arms ? [node.arms.map((arm) => arm.weight)] : [];
    });
  }

  it("keeps a weight a whole number from 1 to 100", async () => {
    const calls = mockApi(flowRoutes({ flow: { graph: split } }));
    const user = userEvent.setup();
    mount();
    const sheet = await openSheetOf(user, /^Random split/);
    const field = within(sheet).getByLabelText("Weight of arm 1");

    await user.clear(field);
    await user.type(field, "500");

    await waitFor(() => expect(saves(calls).length).toBeGreaterThan(0), SAVE);
    for (const saved of weights(calls)) {
      for (const weight of saved) {
        expect(weight).toBeGreaterThanOrEqual(1);
        expect(weight).toBeLessThanOrEqual(100);
      }
    }
    expect(weights(calls).at(-1)?.[0]).toBe(100);
  });

  it("raises zero and a negative weight to 1", async () => {
    const calls = mockApi(flowRoutes({ flow: { graph: split } }));
    const user = userEvent.setup();
    mount();
    const sheet = await openSheetOf(user, /^Random split/);
    const field = within(sheet).getByLabelText("Weight of arm 1");

    await user.clear(field);
    await user.type(field, "-4");

    await waitFor(() => expect(saves(calls).length).toBeGreaterThan(0), SAVE);
    expect(weights(calls).at(-1)?.[0]).toBe(1);
  });

  it("does not save an empty weight as 0, and says what is wanted", async () => {
    const calls = mockApi(flowRoutes({ flow: { graph: split } }));
    const user = userEvent.setup();
    mount();
    const sheet = await openSheetOf(user, /^Random split/);
    const field = within(sheet).getByLabelText("Weight of arm 1");

    await user.clear(field);

    expect(await within(sheet).findByText("Enter a whole number from 1 to 100.")).toBeInTheDocument();
    await pause(1200);
    expect(saves(calls)).toHaveLength(0);
  });
});

describe("a duration", () => {
  it("does not save an empty or negative amount", async () => {
    const calls = mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();
    const sheet = await openSheetOf(user, /^Wait: 1 day/);
    const amount = within(sheet).getByLabelText("Amount");

    await user.clear(amount);
    await user.type(amount, "-3");
    await pause(1200);

    expect(saves(calls)).toHaveLength(0);
  });
});

describe("stale issues", () => {
  it("drops the issues of a deleted step at once and says it is checking", async () => {
    mockApi(flowRoutes({ issues: [{ code: "invalid_url", node: "n4", at: "/url" }] }));
    const user = userEvent.setup();
    mount();
    expect(await screen.findByRole("button", { name: "1 issue" })).toBeInTheDocument();
    const sheet = await openSheetOf(user, /^Webhook: /);

    await user.click(within(sheet).getByRole("button", { name: "Delete step…" }));
    await user.click(await screen.findByRole("button", { name: "Delete step" }));

    expect(screen.queryByRole("button", { name: "1 issue" })).not.toBeInTheDocument();
    expect(screen.queryByText("No issues")).not.toBeInTheDocument();
    expect(screen.getByText("Checking…")).toBeInTheDocument();
  });

  it("ignores a check that was answered after a newer save began", async () => {
    const routes = flowRoutes();
    const save = routes[`PATCH ${WS}/flows/${FLOW_ID}`]!;
    const firstCheck = gate<Response>();
    const firstSave = gate<undefined>();
    let checks = 0;
    let patches = 0;
    const calls = mockApi({
      ...routes,
      [`POST ${WS}/flows/${FLOW_ID}/validate`]: () => {
        checks += 1;
        return checks === 1 ? firstCheck.promise : Promise.resolve(json({ valid: true, issues: [] }));
      },
      [`PATCH ${WS}/flows/${FLOW_ID}`]: async (request) => {
        patches += 1;
        if (patches === 1) await firstSave.promise;
        return save(request);
      },
    });
    const user = userEvent.setup();
    mount();
    const sheet = await openSheetOf(user, /^Wait: 1 day/);
    await waitFor(() => expect(checks).toBe(1));

    await user.selectOptions(within(sheet).getByLabelText("Unit"), "hour");
    await waitFor(() => expect(patches).toBe(1), SAVE);
    await act(async () => {
      firstCheck.release(json({ valid: false, issues: [{ code: "invalid_url", node: "n4", at: "/url" }] }));
      await pause(50);
    });

    expect(screen.queryByRole("button", { name: "1 issue" })).not.toBeInTheDocument();

    firstSave.release(undefined);
    expect(await screen.findByText("No issues", undefined, SAVE)).toBeInTheDocument();
    expect(saves(calls)).toHaveLength(1);
  });
});

describe("a failed save", () => {
  it("keeps saying it is checking while a newer edit waits", async () => {
    const routes = flowRoutes({ issues: [{ code: "invalid_url", node: "n4", at: "/url" }] });
    const save = routes[`PATCH ${WS}/flows/${FLOW_ID}`]!;
    const firstSave = gate<undefined>();
    let patches = 0;
    mockApi({
      ...routes,
      [`PATCH ${WS}/flows/${FLOW_ID}`]: async (request) => {
        patches += 1;
        if (patches === 1) {
          await firstSave.promise;
          return problem(500, "Internal Server Error", "internal error");
        }
        return save(request);
      },
    });
    const user = userEvent.setup();
    mount();
    expect(await screen.findByRole("button", { name: "1 issue" })).toBeInTheDocument();
    const sheet = await openSheetOf(user, /^Wait: 1 day/);

    await user.selectOptions(within(sheet).getByLabelText("Unit"), "hour");
    await waitFor(() => expect(patches).toBe(1), SAVE);
    await user.selectOptions(within(sheet).getByLabelText("Unit"), "minute");
    await act(async () => {
      firstSave.release(undefined);
      await pause(50);
    });

    expect(screen.getByText("Checking…")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "1 issue" })).not.toBeInTheDocument();
    await waitFor(() => expect(patches).toBe(2), SAVE);
    expect(await screen.findByRole("button", { name: "1 issue" }, SAVE)).toBeInTheDocument();
    expect(screen.queryByText("Checking…")).not.toBeInTheDocument();
  });
});

describe("boolean choices", () => {
  it("shows an unchosen trait value as unchosen, not as false", async () => {
    mockApi(
      flowRoutes({
        flow: { graph: graphOf([{ id: "n1", type: "action.update_trait", key: "vip", change: { op: "set", value: "" } }]) },
      }),
    );
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Update a trait: /);

    const value = await within(sheet).findByLabelText("Value");
    expect(value).toHaveValue("");
    expect(within(value).getByRole("option", { name: "Choose…" })).toBeInTheDocument();
  });

  it("shows an unchosen event filter value as unchosen, not as true", async () => {
    const routes = flowRoutes({
      flow: {
        graph: graphOf([
          {
            id: "n1",
            type: "wait.until_event",
            event: "purchase",
            timeout_seconds: 3600,
            filters: [{ prop: "repeat", op: "eq", value: "" }],
          },
        ]),
      },
    });
    const withBoolean: Handler = () =>
      json({
        ...PURCHASE_DETAIL,
        versions: [{ ...PURCHASE_DETAIL.versions[0], properties: [{ name: "repeat", type: "boolean" }] }],
      });
    mockApi({ ...routes, [`GET ${WS}/event-schemas/purchase`]: withBoolean });
    const user = userEvent.setup();
    mount();

    const sheet = await openSheetOf(user, /^Wait for an event: /);

    const filter = await within(sheet).findByRole("group", { name: "Filter 1" });
    await waitFor(() => expect(within(filter).getByLabelText("Value")).toHaveValue(""));
  });
});

describe("the views", () => {
  it("has one tabpanel, labelled by the selected tab, and only that tab controls it", async () => {
    mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();
    const canvas = await screen.findByRole("tab", { name: "Canvas" });

    const panel = screen.getByRole("tabpanel");
    expect(panel).toHaveAttribute("tabindex", "0");
    expect(panel).toHaveAccessibleName("Canvas");
    expect(canvas).toHaveAttribute("aria-controls", panel.id);
    expect(screen.getByRole("tab", { name: "Versions" })).not.toHaveAttribute("aria-controls");

    await user.click(screen.getByRole("tab", { name: "Versions" }));

    const next = screen.getByRole("tabpanel");
    expect(next).toHaveAccessibleName("Versions");
    expect(screen.getByRole("tab", { name: "Versions" })).toHaveAttribute("aria-controls", next.id);
    expect(screen.getByRole("tab", { name: "Canvas" })).not.toHaveAttribute("aria-controls");
  });
});

describe("a connection to a step that is not there", () => {
  it("lists its issue and opens the source step with the problem on that output", async () => {
    const graph = sampleGraph();
    graph.edges = graph.edges.map((edge) => (edge.id === "e4" ? { ...edge, to: "ghost" } : edge));
    mockApi(flowRoutes({ flow: { graph }, issues: [{ code: "unknown_node", edge: "e4" }] }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "1 issue" }));
    await user.click(screen.getByRole("button", { name: /A connection leads to or from a step that is not in the flow/ }));

    const sheet = await screen.findByRole("dialog");
    const select = within(sheet).getByLabelText("Next step");
    expect(select).toHaveValue("ghost");
    expect(select).toHaveAccessibleDescription(/not in the flow/);
    await user.click(within(sheet).getByRole("button", { name: "Disconnect Next" }));
    expect(within(sheet).getByLabelText("Next step")).toHaveValue("");
  });
});

describe("a connection issue that names a path", () => {
  it("still puts the problem on the source step's output", async () => {
    const graph = sampleGraph();
    graph.edges = graph.edges.map((edge) => (edge.id === "e4" ? { ...edge, to: "ghost" } : edge));
    mockApi(flowRoutes({ flow: { graph }, issues: [{ code: "unknown_node", edge: "e4", at: "/to" }] }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "1 issue" }));
    await user.click(screen.getByRole("button", { name: /A connection leads to or from a step that is not in the flow/ }));

    const sheet = await screen.findByRole("dialog");
    expect(within(sheet).getByLabelText("Next step")).toHaveAccessibleDescription(/not in the flow/);
  });
});

describe("saving", () => {
  it("asks before leaving with changes that are not saved", async () => {
    mockApi(flowRoutes());
    const user = userEvent.setup();
    const { router } = mount();
    const sheet = await openSheetOf(user, /^Wait: 1 day/);
    await user.selectOptions(within(sheet).getByLabelText("Unit"), "hour");

    await user.click(screen.getByRole("link", { name: "← Flows" }));

    expect(await screen.findByText("Leave without saving?")).toBeInTheDocument();
    expect(router.state.location.pathname).toBe(`/w/ws1/flows/${FLOW_ID}`);
    await user.click(screen.getByRole("button", { name: "Keep editing" }));
    expect(screen.queryByText("Leave without saving?")).not.toBeInTheDocument();
  });

  it("sends an edit made while a save is in flight after that save", async () => {
    const routes = flowRoutes();
    const save = routes[`PATCH ${WS}/flows/${FLOW_ID}`]!;
    const first = gate<undefined>();
    let patches = 0;
    const calls = mockApi({
      ...routes,
      [`PATCH ${WS}/flows/${FLOW_ID}`]: async (request) => {
        patches += 1;
        if (patches === 1) await first.promise;
        return save(request);
      },
    });
    const user = userEvent.setup();
    mount();
    const sheet = await openSheetOf(user, /^Wait: 1 day/);
    await user.selectOptions(within(sheet).getByLabelText("Unit"), "hour");
    await waitFor(() => expect(patches).toBe(1), SAVE);

    await user.selectOptions(within(sheet).getByLabelText("Unit"), "minute");
    await pause(1000);
    expect(saves(calls)).toHaveLength(1);
    first.release(undefined);

    await waitFor(() => expect(saves(calls)).toHaveLength(2), SAVE);
    expect(lastGraph(calls)?.nodes.find((node) => node.id === "n1")?.["seconds"]).toBe(60);
  });

  it("saves the undone graph after a save that was already in flight", async () => {
    const routes = flowRoutes();
    const save = routes[`PATCH ${WS}/flows/${FLOW_ID}`]!;
    const first = gate<undefined>();
    let patches = 0;
    const calls = mockApi({
      ...routes,
      [`PATCH ${WS}/flows/${FLOW_ID}`]: async (request) => {
        patches += 1;
        if (patches === 1) await first.promise;
        return save(request);
      },
    });
    const user = userEvent.setup();
    mount();
    const sheet = await openSheetOf(user, /^Wait: 1 day/);
    await user.selectOptions(within(sheet).getByLabelText("Unit"), "hour");
    await waitFor(() => expect(patches).toBe(1), SAVE);

    await user.click(screen.getByRole("button", { name: "Undo" }));
    first.release(undefined);

    await waitFor(() => expect(saves(calls)).toHaveLength(2), SAVE);
    expect(lastGraph(calls)?.nodes.find((node) => node.id === "n1")?.["seconds"]).toBe(86_400);
  });
});
