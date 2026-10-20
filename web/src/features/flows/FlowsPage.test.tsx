import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { FLOW_ID, flowBody, flowDetail, WS } from "../../test/flowsApi";
import { json, mockApi, problem } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { actor } from "../../test/segmentsApi";
import { FlowsPage } from "./FlowsPage";
import { NewFlowPage } from "./NewFlowPage";

function mountList() {
  return renderRoute("/w/ws1/flows", "/w/:ws/flows", <FlowsPage />);
}

describe("flows list", () => {
  it("shows a loading state while the flows load", async () => {
    mockApi({ ...actor("writer"), [`GET ${WS}/flows`]: () => new Promise<Response>(() => undefined) });
    mountList();

    expect(await screen.findByText("Loading flows")).toBeInTheDocument();
  });

  it("offers a first flow when there are none and the person can write", async () => {
    mockApi({ ...actor("writer"), [`GET ${WS}/flows`]: () => json([]) });
    mountList();

    expect(await screen.findByText("No flows yet")).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "New flow" })).not.toHaveLength(0);
  });

  it("shows a retry when loading fails, and loads again", async () => {
    let attempts = 0;
    mockApi({
      ...actor("writer"),
      [`GET ${WS}/flows`]: () => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json([flowBody()]);
      },
    });
    const user = userEvent.setup();
    mountList();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByText("Welcome series")).toBeInTheDocument();
    expect(attempts).toBe(2);
  });

  it("lists each flow with its state and live version", async () => {
    mockApi({
      ...actor("writer"),
      [`GET ${WS}/flows`]: () =>
        json([
          flowBody(),
          flowBody({ id: "x2", name: "Win-back", state: "published", current_version: 3 }),
          flowBody({ id: "x3", name: "Old promo", state: "paused", current_version: 1 }),
        ]),
    });
    mountList();

    const table = await screen.findByRole("table", { name: "Flows of this workspace" });
    const rows = within(table).getAllByRole("row");
    expect(within(rows[1]!).getByRole("link", { name: "Welcome series" })).toHaveAttribute("href", `/w/ws1/flows/${FLOW_ID}`);
    expect(within(rows[1]!).getByText("Draft")).toBeInTheDocument();
    expect(within(rows[1]!).getByText("Not published")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("Live")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("Version 3")).toBeInTheDocument();
    expect(within(rows[3]!).getByText("Paused")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "New flow" })).toBeInTheDocument();
  });

  it("hides New flow from someone who cannot write", async () => {
    mockApi({ ...actor("reader"), [`GET ${WS}/flows`]: () => json([flowBody()]) });
    mountList();

    expect(await screen.findByText("Welcome series")).toBeInTheDocument();
    expect(await screen.findByText("You can look at flows but not change them.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "New flow" })).not.toBeInTheDocument();
  });

  it("offers no way to create the first flow to a reader", async () => {
    mockApi({ ...actor("reader"), [`GET ${WS}/flows`]: () => json([]) });
    mountList();

    expect(await screen.findByText("No flows yet")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "New flow" })).not.toBeInTheDocument();
  });
});

describe("new flow", () => {
  function mountNew() {
    return renderRoute("/w/ws1/flows/new", "/w/:ws/flows/new", <NewFlowPage />);
  }

  it("posts the name and the trigger, then opens the editor", async () => {
    const calls = mockApi({
      ...actor("writer"),
      [`POST ${WS}/flows`]: () => json(flowDetail({ graph: { v: 1, nodes: [], edges: [] } }), 201),
    });
    const user = userEvent.setup();
    const { router } = mountNew();

    await user.type(await screen.findByLabelText("Name"), "  Welcome series ");
    await user.selectOptions(screen.getByLabelText("Starts when"), "event");
    await user.click(screen.getByRole("button", { name: "Create flow" }));

    await waitFor(() => expect(router.state.location.pathname).toBe(`/w/ws1/flows/${FLOW_ID}`));
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toMatchObject({
      name: "Welcome series",
      graph: { v: 1, nodes: [], edges: [] },
      settings: { trigger: { type: "event", event: "" }, reentry: { mode: "once" } },
    });
  });

  it("needs a name before it creates", async () => {
    mockApi({ ...actor("writer") });
    mountNew();

    expect(await screen.findByRole("button", { name: "Create flow" })).toBeDisabled();
  });

  it("shows why the API refused and stays on the page", async () => {
    mockApi({ ...actor("writer"), [`POST ${WS}/flows`]: () => problem(409, "Conflict", "A flow with that name exists.") });
    const user = userEvent.setup();
    const { router } = mountNew();

    await user.type(await screen.findByLabelText("Name"), "Welcome series");
    await user.click(screen.getByRole("button", { name: "Create flow" }));

    expect(await screen.findByText("A flow with that name exists.")).toBeInTheDocument();
    expect(router.state.location.pathname).toBe("/w/ws1/flows/new");
  });

  it("tells a reader they cannot create flows", async () => {
    mockApi({ ...actor("reader") });
    mountNew();

    expect(await screen.findByText("You cannot create flows")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Create flow" })).not.toBeInTheDocument();
  });
});
