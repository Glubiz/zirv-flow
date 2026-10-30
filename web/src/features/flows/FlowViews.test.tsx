import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { FLOW_ID, flowRoutes, sampleGraph, sampleSettings, versionBody, WS } from "../../test/flowsApi";
import { lastGraph, mountEditor as mount, openInspectorOf } from "../../test/flowsEditor";
import { json, mockApi, problem } from "../../test/mockApi";
import { actor } from "../../test/segmentsApi";
import { stubReactFlow } from "../../test/reactFlowStubs";
import { insertNode, TRIGGER_ID, type Graph } from "./model";

beforeEach(stubReactFlow);

const FLOW = `${WS}/flows/${FLOW_ID}`;
const SAVE = { timeout: 3000 };
const tally = {
  node_id: "n1", node_type: "wait.duration", entered: 12, completed: 7, waiting: 3, failed: 2,
  sent: 0, send_failed: 0, skipped: [], goal_conversions: 0,
};

function resultsRoutes(nodes = [tally]) {
  return {
    ...flowRoutes({ flow: { state: "published", current_version: 2 } }),
    [`GET ${FLOW}/versions`]: () => json([versionBody(2)]),
    [`GET ${FLOW}/versions/2/counts`]: () => json({ version: 2, nodes }),
    [`GET ${FLOW}/enrollments`]: () => json({ enrollments: [], next_cursor: null }),
  };
}

describe("flow view addresses", () => {
  it.each([
    ["", "Build"], ["?view=test", "Test"], ["?view=results", "Results"],
    ["?tab=canvas", "Build"], ["?tab=settings", "Build"], ["?tab=enrollments", "Results"],
  ])("reads %s as %s", async (query, view) => {
    mockApi(flowRoutes());
    mount(query);
    expect(await screen.findByRole("link", { name: view })).toHaveAttribute("aria-current", "page");
  });

  it("writes the view to the URL, preserves other parameters and follows browser history", async () => {
    mockApi(flowRoutes());
    const user = userEvent.setup();
    const { router } = mount("?from=overview");
    await user.click(await screen.findByRole("link", { name: "Test" }));
    expect(router.state.location.search).toBe("?from=overview&view=test");
    await user.click(screen.getByRole("link", { name: "Results" }));
    expect(router.state.location.search).toBe("?from=overview&view=results");
    await act(() => router.navigate(-1));
    expect(screen.getByRole("link", { name: "Test" })).toHaveAttribute("aria-current", "page");
    await user.click(screen.getByRole("link", { name: "Build" }));
    expect(router.state.location.search).toBe("?from=overview");
  });

  it("keeps an old versions address reachable from Build", async () => {
    mockApi(flowRoutes());
    mount("?tab=versions");
    expect(await screen.findByRole("region", { name: "Published versions" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Build" })).toHaveAttribute("aria-current", "page");
  });
});

describe("the Build workbench", () => {
  it("filters the palette and inserts with Enter after the selected step, preserving its continuation", async () => {
    const calls = mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();
    await openInspectorOf(user, /^Wait: 1 day/);
    const palette = screen.getByRole("complementary", { name: "Step palette" });
    await user.type(within(palette).getByLabelText("Filter steps"), "webhook");
    expect(within(palette).getAllByRole("button")).toHaveLength(1);
    const add = within(palette).getByRole("button", { name: "Webhook" });
    add.focus();
    await user.keyboard("{Enter}");
    await waitFor(() => {
      expect(lastGraph(calls)?.edges).toEqual(expect.arrayContaining([
        expect.objectContaining({ from: "n1", port: "next", to: "n6" }),
        expect.objectContaining({ from: "n6", port: "next", to: "n2" }),
      ]));
    }, SAVE);
    expect(lastGraph(calls)?.edges.some((edge) => edge.from === "n1" && edge.to === "n2")).toBe(false);
    expect(screen.getByRole("region", { name: "Step inspector" })).toHaveFocus();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    await user.clear(within(palette).getByLabelText("Filter steps"));
    await user.type(within(palette).getByLabelText("Filter steps"), "no-such-step");
    expect(within(palette).getByRole("status")).toHaveTextContent("No steps match");
  });

  it("opens flow settings from the trigger while the canvas stays rendered", async () => {
    mockApi(flowRoutes());
    const user = userEvent.setup();
    mount();
    await openInspectorOf(user, /^Wait: 1 day/);
    const canvas = screen.getByRole("region", { name: "Flow canvas" });
    expect(screen.getByRole("region", { name: "Step inspector" }).closest(".fl-sheet")).toBeNull();
    await user.click(screen.getByRole("group", { name: /^Trigger:/ }));
    const settings = screen.getByRole("region", { name: "Flow settings" });
    expect(within(settings).getByLabelText("Starts when")).toHaveValue("segment");
    expect(within(settings).getByRole("heading", { name: "Trigger" })).toHaveFocus();
    expect(screen.getByRole("region", { name: "Flow canvas" })).toBe(canvas);
  });

  it("starts an empty flow with its trigger and inserts the first step", async () => {
    const calls = mockApi(flowRoutes({ flow: { graph: { v: 1, nodes: [], edges: [] } } }));
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("group", { name: /^Trigger:/ }));
    expect(screen.getByText("Start with the trigger, then add your first step.")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Wait" }));
    await waitFor(() => expect(lastGraph(calls)?.nodes).toHaveLength(1), SAVE);
    expect(screen.getByRole("group", { name: /^Wait: 1 day, step 1 of 1/ })).toBeInTheDocument();
  });

  it("renders the problem on its node and blocks review until validation clears it", async () => {
    mockApi(flowRoutes({ issues: (asked) => asked === 1 ? [{ code: "invalid_url", node: "n4", at: "/url" }] : [] }));
    const user = userEvent.setup();
    mount();
    const hook = await screen.findByRole("group", { name: /^Webhook: .*1 issue$/ });
    expect(within(hook).getByText(/web address is not valid/)).toBeInTheDocument();
    const publish = screen.getByRole("button", { name: "Review and publish (1)" });
    expect(publish).toHaveAttribute("aria-disabled", "true");
    expect(publish).toHaveAccessibleDescription("1 problem must be fixed before this flow can be published.");
    await user.click(publish);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const inspector = await openInspectorOf(user, /^Webhook:/);
    await user.type(within(inspector).getByLabelText("URL"), "/fixed");
    await waitFor(() => expect(screen.getByRole("button", { name: "Review and publish (0)" })).not.toHaveAttribute("aria-disabled"), SAVE);
  });

  it("keeps the publish button focusable while blocked and shows no count before validation returns", async () => {
    mockApi({ ...flowRoutes({ issues: [{ code: "invalid_url", node: "n4", at: "/url" }] }), [`POST ${FLOW}/validate`]: () => new Promise<Response>(() => undefined) });
    mount();
    expect(await screen.findByRole("button", { name: "Review and publish" })).toBeEnabled();
    expect(screen.queryByRole("button", { name: /Review and publish \(/ })).not.toBeInTheDocument();
  });

  it("lets a keyboard user focus the blocked publish button and read why", async () => {
    mockApi(flowRoutes({ issues: [{ code: "invalid_url", node: "n4", at: "/url" }] }));
    mount();
    const publish = await screen.findByRole("button", { name: "Review and publish (1)" });
    publish.focus();
    expect(publish).toHaveFocus();
    expect(publish).not.toBeDisabled();
    expect(publish).toHaveAccessibleDescription(/1 problem/);
  });

  it("labels counts with their version and reads this step's results with the keyboard", async () => {
    mockApi(resultsRoutes());
    const user = userEvent.setup();
    mount();
    const toggle = await screen.findByLabelText("Show counts from version 2");
    await user.click(toggle);
    expect(await screen.findByRole("group", { name: /^Wait: .*12 entered, 3 waiting, 2 failed/ })).toBeInTheDocument();
    const inspector = await openInspectorOf(user, /^Wait:/);
    within(inspector).getByRole("tab", { name: "Fields" }).focus();
    await user.keyboard("{ArrowRight}");
    expect(within(inspector).getByRole("tab", { name: "This step's results" })).toHaveFocus();
    expect(await within(inspector).findByText("Results from version 2")).toBeInTheDocument();
    expect(within(inspector).getByText("12")).toBeInTheDocument();
    await user.click(toggle);
    expect(screen.getByRole("group", { name: /^Wait:/ })).not.toHaveAccessibleName(/12 entered/);
  });

  it("shows draft changes only after comparing the published version", async () => {
    const graph = sampleGraph();
    mockApi({
      ...flowRoutes({ flow: { state: "published", current_version: 2, graph, settings: sampleSettings() } }),
      [`GET ${FLOW}/versions`]: () => json([versionBody(2, { graph: { ...graph, nodes: [{ id: "n1", type: "exit" }], edges: [] } })]),
    });
    mount();
    expect(await screen.findByText("Draft with changes")).toBeInTheDocument();
    expect(screen.getByText("v2 live")).toBeInTheDocument();
  });
});

describe("Results", () => {
  it("renders totals and the step table from counts, with enrollments below", async () => {
    mockApi(resultsRoutes());
    mount("?view=results");
    const table = await screen.findByRole("table", { name: "Counts per step of version 2" });
    expect(within(table).getByRole("row", { name: /Wait: 1 day 12 3 2 0 None/ })).toBeInTheDocument();
    const totals = screen.getByLabelText("Totals for version 2");
    expect(totals).toHaveTextContent("Step runs12");
    expect(totals).toHaveTextContent("Waiting3");
    expect(await screen.findByText("No one has entered this flow yet")).toBeInTheDocument();
    expect(screen.getByRole("region", { name: "Enrollments" })).toBeInTheDocument();
  });

  it("shows an honest empty state for a published version with no runs", async () => {
    mockApi(resultsRoutes([{ ...tally, entered: 0, completed: 0, waiting: 0, failed: 0 }]));
    mount("?view=results");
    expect(await screen.findByText("No results yet")).toBeInTheDocument();
    expect(screen.queryByText(/NaN/)).not.toBeInTheDocument();
  });

  it("keeps an error distinct from zero results", async () => {
    mockApi({ ...resultsRoutes(), [`GET ${FLOW}/versions/2/counts`]: () => problem(500, "Unavailable", "Counts are unavailable") });
    mount("?view=results");
    expect(await screen.findByText("Counts are unavailable")).toBeInTheDocument();
    expect(screen.queryByText("No results yet")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("Totals for version 2")).not.toBeInTheDocument();
  });

  it("looks up a person and links to their existing messages view", async () => {
    mockApi({ ...resultsRoutes(), [`POST ${WS}/people/search`]: () => json({ people: [{ id: "person-1", display: { value: "reader@example.test" } }], next_cursor: null }) });
    const user = userEvent.setup();
    mount("?view=results");
    const lookup = await screen.findByRole("region", { name: "Person lookup" });
    await user.type(within(lookup).getByLabelText("Search people"), "reader@example.test");
    await user.click(within(lookup).getByRole("button", { name: "Search" }));
    await user.click(await within(lookup).findByRole("button", { name: "Use reader@example.test" }));
    expect(within(lookup).getByRole("link", { name: "Messages for reader@example.test" })).toHaveAttribute("href", "/w/ws1/people/person-1?tab=messages");
  });
});

describe("Results person lookup access", () => {
  it("hides the lookup from a reader without the personal data grant", async () => {
    mockApi({ ...resultsRoutes(), ...actor("reader") });
    mount("?view=results");
    await screen.findByRole("table", { name: "Counts per step of version 2" });
    expect(screen.queryByRole("region", { name: "Person lookup" })).not.toBeInTheDocument();
  });

  it("names the counts toggle once and carries its state in aria-pressed", async () => {
    mockApi(resultsRoutes());
    const user = userEvent.setup();
    mount("?view=results");
    const toggle = await screen.findByRole("button", { name: "Counts on canvas" });
    expect(toggle).toHaveAttribute("aria-pressed", "true");
    await user.click(toggle);
    expect(screen.getByRole("button", { name: "Counts on canvas" })).toHaveAttribute("aria-pressed", "false");
  });
});

describe("insertion boundaries", () => {
  const graph = sampleGraph() as Graph;
  it("inserts before the entry when the trigger is selected", () => {
    const added = insertNode(graph, "wait.duration", TRIGGER_ID);
    expect(added.graph.nodes[0]?.id).toBe(added.id);
    expect(added.graph.edges).toContainEqual(expect.objectContaining({ from: added.id, port: "next", to: "n1" }));
  });
  it("uses the first branch output without replacing the other branch", () => {
    const added = insertNode(graph, "wait.duration", "n2");
    expect(added.graph.edges).toContainEqual(expect.objectContaining({ from: "n2", port: "case_0", to: added.id }));
    expect(added.graph.edges).toContainEqual(expect.objectContaining({ from: "n2", port: "else", to: "n4" }));
    expect(added.graph.edges).toContainEqual(expect.objectContaining({ from: added.id, port: "next", to: "n3" }));
  });
  it("does not invent an output on an exit", () => {
    const added = insertNode(graph, "wait.duration", "n5");
    expect(added.graph.edges).toEqual(graph.edges);
    expect(added.graph.nodes).toHaveLength(graph.nodes.length + 1);
  });
});
