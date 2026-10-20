import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { FLOW_ID, flowRoutes, versionBody, WS } from "../../test/flowsApi";
import { mountEditor as mount } from "../../test/flowsEditor";
import { json, mockApi, problem } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

beforeEach(() => {
  stubReactFlow();
});

const VERSIONS = `GET ${WS}/flows/${FLOW_ID}/versions`;

/** Version 1: a two day wait and the exit, which the draft does not have. */
function olderGraph() {
  return {
    v: 1,
    nodes: [
      { id: "n1", type: "wait.duration", seconds: 172_800 },
      { id: "n2", type: "exit" },
    ],
    edges: [{ id: "e1", from: "n1", port: "next", to: "n2" }],
  };
}

async function openVersions(user: ReturnType<typeof userEvent.setup>) {
  await screen.findByRole("group", { name: /^Wait: 1 day/ });
  await user.click(screen.getByRole("tab", { name: "Versions" }));
  return screen.findByRole("tabpanel");
}

describe("versions list", () => {
  it("shows a loading state", async () => {
    mockApi({ ...flowRoutes(), [VERSIONS]: () => new Promise<Response>(() => undefined) });
    const user = userEvent.setup();
    mount();
    await openVersions(user);

    expect(await screen.findByText("Loading the versions")).toBeInTheDocument();
  });

  it("says a flow is not published yet, and offers Publish… to an admin only", async () => {
    mockApi({ ...flowRoutes(), [VERSIONS]: () => json([]) });
    const user = userEvent.setup();
    const view = mount();
    const panel = await openVersions(user);

    expect(await within(panel).findByText("Not published yet")).toBeInTheDocument();
    expect(within(panel).getByRole("button", { name: "Publish…" })).toBeInTheDocument();
    view.unmount();

    mockApi({ ...flowRoutes({ role: "member" }), [VERSIONS]: () => json([]) });
    mount();
    const memberPanel = await openVersions(user);

    expect(await within(memberPanel).findByText("Not published yet")).toBeInTheDocument();
    expect(within(memberPanel).queryByRole("button", { name: "Publish…" })).not.toBeInTheDocument();
  });

  it("shows an error with a way to try again", async () => {
    let asked = 0;
    mockApi({
      ...flowRoutes(),
      [VERSIONS]: () => {
        asked += 1;
        return asked === 1 ? problem(500, "Internal Server Error", "the versions could not be read") : json([versionBody(1)]);
      },
    });
    const user = userEvent.setup();
    mount();
    const panel = await openVersions(user);

    expect(await within(panel).findByText("the versions could not be read")).toBeInTheDocument();
    await user.click(within(panel).getByRole("button", { name: "Try again" }));

    expect(await within(panel).findByText("Version 1")).toBeInTheDocument();
  });

  it("lists the versions newest first, with the publisher and which one is live", async () => {
    mockApi({
      ...flowRoutes({ flow: { state: "published", current_version: 2 } }),
      [VERSIONS]: () => json([versionBody(1, { graph: olderGraph(), published_by: "u-member" }), versionBody(2)]),
    });
    const user = userEvent.setup();
    mount();
    const panel = await openVersions(user);

    const rows = await within(panel).findAllByRole("listitem");
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveTextContent("Version 2");
    expect(rows[0]).toHaveTextContent("Live");
    expect(rows[0]).toHaveTextContent("by an API key");
    expect(rows[1]).toHaveTextContent("Version 1");
    expect(rows[1]).not.toHaveTextContent("Live");
    expect(rows[1]).toHaveTextContent("by member@example.test");
  });
});

describe("a version on the canvas", () => {
  async function viewVersionOne(user: ReturnType<typeof userEvent.setup>) {
    mockApi({
      ...flowRoutes({ flow: { state: "published", current_version: 2 } }),
      [VERSIONS]: () => json([versionBody(1, { graph: olderGraph() }), versionBody(2)]),
    });
    mount();
    await openVersions(user);
    await user.click(await screen.findByRole("button", { name: "View version 1" }));
  }

  it("draws the version read-only, with a banner", async () => {
    const user = userEvent.setup();
    await viewVersionOne(user);

    expect(await screen.findByText("Viewing version 1 · read-only")).toBeInTheDocument();
    expect(await screen.findByRole("group", { name: /^Wait: 2 days/ })).toBeInTheDocument();
    expect(screen.queryByRole("group", { name: /^Wait: 1 day/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add a step" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Undo" })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Fit to screen" })).toBeInTheDocument();
  });

  it("announces the banner through the page's polite region, which is there before the words", async () => {
    const user = userEvent.setup();
    mockApi({
      ...flowRoutes({ flow: { state: "published", current_version: 2 } }),
      [VERSIONS]: () => json([versionBody(1, { graph: olderGraph() }), versionBody(2)]),
    });
    mount();
    await openVersions(user);
    await screen.findByRole("tabpanel");
    const polite = document.querySelector('[aria-live="polite"]')!;
    expect(polite).toBeEmptyDOMElement();

    await user.click(await screen.findByRole("button", { name: "View version 1" }));

    const banner = await screen.findByText("Viewing version 1 · read-only");
    expect(banner).not.toHaveAttribute("role");
    await waitFor(() => expect(polite).toHaveTextContent("Viewing version 1, read-only."));
  });

  it("opens a step in a sheet that cannot change anything", async () => {
    const user = userEvent.setup();
    await viewVersionOne(user);

    await user.click(await screen.findByRole("group", { name: /^Wait: 2 days/ }));

    const sheet = await screen.findByRole("dialog");
    expect(within(sheet).getByLabelText("Amount")).toBeDisabled();
    expect(within(sheet).queryByRole("button", { name: "Delete step…" })).not.toBeInTheDocument();
  });

  it("goes back to the draft, which can be edited again", async () => {
    const user = userEvent.setup();
    await viewVersionOne(user);
    await screen.findByText("Viewing version 1 · read-only");

    await user.click(screen.getByRole("button", { name: "Back to the draft" }));

    expect(await screen.findByRole("group", { name: /^Wait: 1 day/ })).toBeInTheDocument();
    expect(screen.queryByText(/Viewing version [0-9]+ · read-only/)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add a step" })).toBeInTheDocument();
  });

  it("goes back to the list of versions", async () => {
    const user = userEvent.setup();
    await viewVersionOne(user);
    await screen.findByText("Viewing version 1 · read-only");

    await user.click(screen.getByRole("button", { name: "All versions" }));

    expect(await screen.findByRole("button", { name: "View version 2" })).toBeInTheDocument();
  });
});
