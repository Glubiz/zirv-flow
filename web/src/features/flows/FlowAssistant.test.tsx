import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { aiSettings, FLOW_ID, flowRoutes, versionBody, WS } from "../../test/flowsApi";
import { mountEditor as mount, saves, type User } from "../../test/flowsEditor";
import { json, mockApi, problem, type Call, type Handler } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

beforeEach(() => {
  stubReactFlow();
});

const FLOW = `${WS}/flows/${FLOW_ID}`;
const DRAFT = `POST ${WS}/ai/drafts`;
const ACCEPT = `POST ${WS}/ai/drafts/d1/accept`;
const EXPLAIN = `POST ${FLOW}/ai/explain`;

/** The flow the assistant drafts: an hour's wait, then the exit. */
const CONTENT = {
  graph: {
    v: 1,
    nodes: [
      { id: "a1", type: "wait.duration", seconds: 3600 },
      { id: "z9", type: "exit" },
    ],
    edges: [{ id: "x1", from: "a1", port: "next", to: "z9" }],
  },
  settings: { trigger: { type: "manual" }, reentry: { mode: "once" } },
};

const SIMULATION = {
  start_at: "2026-10-21T09:00:00Z",
  not_simulated: ["goal_segment"],
  unreached_nodes: ["ghost"],
  people: [
    {
      description: "A person nothing is known about",
      label: "blank",
      path: ["a1", "z9"],
      final_node: "z9",
      final_status: "exited",
      internal_effects: 0,
      external_effects: 0,
    },
    {
      description: "A person who fits the first condition",
      label: "first_case",
      path: ["a1"],
      final_node: "a1",
      final_status: "waiting",
      internal_effects: 2,
      external_effects: 1,
    },
  ],
};

const VALID = {
  id: "d1",
  kind: "flow",
  status: "valid",
  attempts: 1,
  content: CONTENT,
  issues: [],
  changes: [{ path: "/graph/nodes/a1", change: "added", before: null, after: CONTENT.graph.nodes[0] }],
  simulation: SIMULATION,
};

const EXPLAINED = {
  source: "draft",
  summary: "Everyone waits a day, then the plan decides.",
  nodes: [
    { id: "n2", text: "Sends people who live in Aarhus one way." },
    { id: "n4", text: "Calls the CRM." },
    { id: "gone", text: "A step that is no longer there." },
  ],
};

interface Options {
  role?: "writer" | "member" | "reader";
  enabled?: boolean;
  routes?: Record<string, Handler>;
}

function routes({ role = "writer", enabled = true, routes: extra = {} }: Options = {}): Record<string, Handler> {
  return {
    ...flowRoutes({ role }),
    [`GET ${WS}/ai/settings`]: () => json(aiSettings(enabled)),
    [DRAFT]: () => json(VALID, 201),
    [ACCEPT]: () =>
      json({ id: "d1", kind: "flow", flow_id: FLOW_ID, content: CONTENT, flow: { id: FLOW_ID, name: "Welcome series", ...CONTENT } }),
    [EXPLAIN]: () => json(EXPLAINED),
    ...extra,
  };
}

async function openAssistant(user: User) {
  await screen.findByRole("group", { name: /^Wait: 1 day/ });
  await user.click(await screen.findByRole("button", { name: "Assistant" }));
  return screen.findByRole("dialog", { name: "Assistant" });
}

async function draftIt(user: User, sheet: HTMLElement) {
  await user.type(await within(sheet).findByLabelText("What do you want?"), "Wait an hour, then end");
  await user.click(within(sheet).getByRole("button", { name: "Draft" }));
  return within(sheet).findByRole("heading", { name: "Draft ready" });
}

const calledWith = (calls: Call[], method: string, suffix: string) =>
  calls.filter((call) => call.method === method && call.path.endsWith(suffix));

describe("the assistant in the flow editor", () => {
  it("opens from the tool rail and shows the diff, the simulation summary and accept", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mount();

    const sheet = await openAssistant(user);
    expect(screen.getByRole("button", { name: "Assistant" })).toHaveAttribute("aria-pressed", "true");
    await draftIt(user, sheet);

    expect(calledWith(calls, "POST", "/ai/drafts")[0]?.body).toMatchObject({ kind: "flow", flow_id: FLOW_ID });
    expect(within(sheet).getByRole("table", { name: "What the draft changes" })).toBeInTheDocument();
    const summary = within(sheet).getByRole("group", { name: "Simulation of the draft" });
    expect(within(summary).getByText("A person nothing is known about")).toBeInTheDocument();
    expect(within(summary).getByText("Exited")).toBeInTheDocument();
    expect(within(summary).getByText("Waiting")).toBeInTheDocument();
    // The paths use the names the canvas uses, taken from the drafted flow, with the ids beside them.
    expect(within(summary).getByRole("list", { name: "Path of A person nothing is known about" })).toHaveTextContent(
      /1\. Wait: 1 hour\s*a1.*2\. Exit.*z9/,
    );
    expect(within(summary).getByText(/Would change 2 things about them and make 1 outward call/)).toBeInTheDocument();
    expect(within(summary).getByRole("list", { name: "Steps no one reached" })).toHaveTextContent("ghost");
    expect(within(summary).getByText(/A goal that is reaching a segment is not checked/)).toBeInTheDocument();
    expect(within(sheet).getByRole("button", { name: "Accept" })).toBeEnabled();
  });

  it("accepts into the editor's draft, saves nothing more and never publishes", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mount();
    const sheet = await openAssistant(user);
    await draftIt(user, sheet);

    await user.click(within(sheet).getByRole("button", { name: "Accept" }));

    expect(await within(sheet).findByText(/Loaded into the editor and saved as the flow's draft. Nothing is published./)).toBeInTheDocument();
    expect(await screen.findByRole("group", { name: /^Wait: 1 hour/ })).toBeInTheDocument();
    expect(screen.queryByRole("group", { name: /^Wait: 1 day/ })).not.toBeInTheDocument();
    expect(calledWith(calls, "POST", "/ai/drafts/d1/accept")).toHaveLength(1);
    await new Promise((resolve) => setTimeout(resolve, 1200));
    expect(calls.some((call) => call.path.endsWith("/publish"))).toBe(false);
    // The server wrote the draft, so the editor has nothing of its own to save.
    expect(saves(calls)).toHaveLength(0);
    expect(screen.queryByText(/Couldn.t save/)).not.toBeInTheDocument();
  });

  it("asks before replacing edits that are not saved, and accepts nothing when they are kept", async () => {
    const save = flowRoutes()[`PATCH ${FLOW}`]!;
    let patches = 0;
    const calls = mockApi(
      routes({
        routes: {
          [`PATCH ${FLOW}`]: (request) => {
            patches += 1;
            return patches === 1 ? problem(500, "Internal Server Error", "the save failed") : save(request);
          },
        },
      }),
    );
    const user = userEvent.setup();
    mount();
    await screen.findByRole("group", { name: /^Wait: 1 day/ });
    await user.click(await screen.findByRole("group", { name: /^Wait: 1 day/ }));
    const step = await screen.findByRole("dialog");
    const amount = within(step).getByLabelText("Amount");
    await user.clear(amount);
    await user.type(amount, "3");
    expect(await screen.findByText(/Couldn.t save/, undefined, { timeout: 3000 })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Assistant" }));
    const sheet = await screen.findByRole("dialog", { name: "Assistant" });
    await draftIt(user, sheet);
    await user.click(within(sheet).getByRole("button", { name: "Accept" }));

    const dialog = await screen.findByRole("dialog", { name: "Replace your unsaved changes?" });
    await user.click(within(dialog).getByRole("button", { name: "Keep my changes" }));
    expect(calledWith(calls, "POST", "/ai/drafts/d1/accept")).toHaveLength(0);
    expect(screen.getByText(/Couldn.t save/)).toBeInTheDocument();

    await user.click(within(sheet).getByRole("button", { name: "Accept" }));
    const again = await screen.findByRole("dialog", { name: "Replace your unsaved changes?" });
    await user.click(within(again).getByRole("button", { name: "Replace with the draft" }));

    await waitFor(() => expect(calledWith(calls, "POST", "/ai/drafts/d1/accept")).toHaveLength(1));
    expect(await screen.findByRole("group", { name: /^Wait: 1 hour/ })).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByText(/Couldn.t save/)).not.toBeInTheDocument());
    expect(patches).toBe(1);
  });

  it("is not offered when the assistant is off, and not to a member who is not an admin", async () => {
    mockApi(routes({ enabled: false }));
    const off = mount();
    await screen.findByRole("group", { name: /^Wait: 1 day/ });
    await waitFor(() => expect(screen.getByRole("button", { name: "Fit to screen" })).toBeInTheDocument());
    expect(screen.queryByRole("button", { name: "Assistant" })).not.toBeInTheDocument();
    off.unmount();

    const calls = mockApi(routes({ role: "member" }));
    mount();
    await screen.findByRole("group", { name: /^Wait: 1 day/ });
    expect(screen.queryByRole("button", { name: "Assistant" })).not.toBeInTheDocument();
    // The settings are an admin's to read, so a member's editor never asks.
    expect(calls.some((call) => call.path.endsWith("/ai/settings"))).toBe(false);
  });

  it("shows a refused flow with the server's reason and an invalid draft without an accept button", async () => {
    let attempt = 0;
    mockApi(
      routes({
        routes: {
          [DRAFT]: () => {
            attempt += 1;
            return attempt === 1
              ? problem(422, "Unprocessable Entity", "this flow is too large to send", "t-1", "ai_flow_too_large")
              : json({
                  ...VALID,
                  status: "invalid",
                  content: null,
                  simulation: null,
                  changes: [],
                  issues: [{ code: "unknown_trait", at: "nodes/check/conditions/0/trait" }],
                });
          },
        },
      }),
    );
    const user = userEvent.setup();
    mount();
    const sheet = await openAssistant(user);
    await user.type(await within(sheet).findByLabelText("What do you want?"), "Do something");

    await user.click(within(sheet).getByRole("button", { name: "Draft" }));
    const alert = await within(sheet).findByRole("alert");
    expect(alert).toHaveTextContent("This flow is too large for the assistant");

    await user.click(within(sheet).getByRole("button", { name: "Draft" }));
    expect(await within(sheet).findByRole("heading", { name: /No valid draft after 1 attempt/ })).toBeInTheDocument();
    expect(within(sheet).getByRole("list", { name: "Problems with the last attempt" })).toHaveTextContent("unknown trait");
    expect(within(sheet).queryByRole("button", { name: "Accept" })).not.toBeInTheDocument();
  });
});

describe("explaining a flow", () => {
  it("explains the draft and focuses a cited step on the canvas", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mount();
    const sheet = await openAssistant(user);

    await user.click(within(sheet).getByRole("button", { name: "Explain the draft" }));

    expect(await within(sheet).findByText("Everyone waits a day, then the plan decides.")).toBeInTheDocument();
    expect(calledWith(calls, "POST", "/ai/explain")[0]?.body).toBeNull();
    // A cited step the canvas no longer has is text, not a button.
    expect(within(sheet).getByText(/A step that is no longer there/)).toBeInTheDocument();
    expect(within(sheet).queryByRole("button", { name: /gone/ })).not.toBeInTheDocument();

    await user.click(within(sheet).getByRole("button", { name: /^Show on the canvas: Branch/ }));

    await waitFor(() => expect(screen.getByRole("group", { name: /^Branch: / })).toHaveFocus());
    expect(calledWith(calls, "POST", "/ai/explain")).toHaveLength(1);
  });

  it("says why when the assistant cannot explain, and can ask again", async () => {
    let asked = 0;
    mockApi(
      routes({
        routes: {
          [EXPLAIN]: () => {
            asked += 1;
            return asked === 1 ? problem(402, "Payment Required", "budget", "t-2", "ai_budget_exceeded") : json(EXPLAINED);
          },
        },
      }),
    );
    const user = userEvent.setup();
    mount();
    const sheet = await openAssistant(user);

    await user.click(within(sheet).getByRole("button", { name: "Explain the draft" }));
    expect(await within(sheet).findByText("The monthly AI budget is spent")).toBeInTheDocument();

    await user.click(within(sheet).getByRole("button", { name: "Explain again" }));
    expect(await within(sheet).findByText("Everyone waits a day, then the plan decides.")).toBeInTheDocument();
  });

  it("explains a version when the versions view is showing one, with the version in the request", async () => {
    const calls = mockApi(
      routes({
        routes: {
          [`GET ${FLOW}/versions`]: () => json([versionBody(1)]),
          [EXPLAIN]: () => json({ source: "version", version: 1, summary: "Version one waits, then branches.", nodes: [{ id: "n2", text: "Splits by city." }] }),
        },
      }),
    );
    const user = userEvent.setup();
    mount();
    await screen.findByRole("group", { name: /^Wait: 1 day/ });
    await user.click(screen.getByRole("tab", { name: "Versions" }));
    await user.click(await screen.findByRole("button", { name: "View version 1" }));
    await screen.findByText("Viewing version 1 · read-only");

    await user.click(await screen.findByRole("button", { name: "Assistant" }));
    const sheet = await screen.findByRole("dialog", { name: "Assistant" });
    // A version is read-only: there is nothing to draft over.
    expect(within(sheet).queryByRole("button", { name: "Draft" })).not.toBeInTheDocument();
    await user.click(within(sheet).getByRole("button", { name: "Explain version 1" }));

    expect(await within(sheet).findByText("Version one waits, then branches.")).toBeInTheDocument();
    expect(calledWith(calls, "POST", "/ai/explain")[0]?.body).toEqual({ version: 1 });
    await user.click(within(sheet).getByRole("button", { name: /^Show on the canvas: Branch/ }));
    await waitFor(() => expect(screen.getByRole("group", { name: /^Branch: / })).toHaveFocus());
  });
});
