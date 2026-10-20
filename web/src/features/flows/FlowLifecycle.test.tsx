import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { FLOW_ID, flowDetail, flowRoutes, impactBody, WS } from "../../test/flowsApi";
import { mountEditor as mount } from "../../test/flowsEditor";
import { json, mockApi, problem } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

beforeEach(() => {
  stubReactFlow();
});

const ACTIONS = ["Publish…", "Pause", "Resume", "Archive…"];

function after(state: string, version: number | null = 2) {
  return () => json(flowDetail({ state, current_version: version }));
}

function posted(calls: { method: string; path: string }[], action: string) {
  return calls.filter((call) => call.method === "POST" && call.path === `${WS}/flows/${FLOW_ID}/${action}`);
}

describe("lifecycle actions are for admins", () => {
  it.each(["member", "reader"] as const)("offers a %s none of publish, pause, resume or archive", async (role) => {
    for (const state of ["published", "paused"]) {
      mockApi(flowRoutes({ role, flow: { state, current_version: 2 } }));
      const view = mount();
      await screen.findByRole("group", { name: /^Wait: 1 day/ });
      for (const name of ACTIONS) {
        expect(screen.queryByRole("button", { name })).not.toBeInTheDocument();
      }
      view.unmount();
    }
  });

  it("offers only the actions the state allows", async () => {
    const expected: Record<string, string[]> = {
      draft: ["Publish…", "Archive…"],
      published: ["Publish…", "Pause", "Archive…"],
      paused: ["Publish…", "Resume", "Archive…"],
      archived: [],
    };
    for (const [state, names] of Object.entries(expected)) {
      mockApi(flowRoutes({ flow: { state, current_version: state === "draft" ? null : 2 } }));
      const view = mount();
      await screen.findByRole("group", { name: /^Wait: 1 day/ });
      for (const name of ACTIONS) {
        if (names.includes(name)) expect(screen.getByRole("button", { name })).toBeEnabled();
        else expect(screen.queryByRole("button", { name })).not.toBeInTheDocument();
      }
      view.unmount();
    }
  });
});

describe("pause and resume", () => {
  it("pauses a live flow and resumes it", async () => {
    const calls = mockApi({
      ...flowRoutes({ flow: { state: "published", current_version: 2 } }),
      [`POST ${WS}/flows/${FLOW_ID}/pause`]: after("paused"),
      [`POST ${WS}/flows/${FLOW_ID}/resume`]: after("published"),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Pause" }));

    expect(await screen.findByText("Paused")).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "Resume" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Pause" })).not.toBeInTheDocument();
    expect(screen.getByText("The flow is paused.")).toBeInTheDocument();
    expect(posted(calls, "pause")).toHaveLength(1);

    await user.click(screen.getByRole("button", { name: "Resume" }));

    expect(await screen.findByText("Live")).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "Pause" })).toBeInTheDocument();
    expect(posted(calls, "resume")).toHaveLength(1);
  });

  it("refetches and says so when the state changed elsewhere", async () => {
    const routes = flowRoutes({ flow: { state: "published", current_version: 2 } });
    const load = routes[`GET ${WS}/flows/${FLOW_ID}`]!;
    let loads = 0;
    mockApi({
      ...routes,
      [`GET ${WS}/flows/${FLOW_ID}`]: (request) => {
        loads += 1;
        return loads === 1 ? load(request) : json(flowDetail({ state: "paused", current_version: 2 }));
      },
      [`POST ${WS}/flows/${FLOW_ID}/pause`]: () => problem(409, "Conflict", "The flow is not published."),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Pause" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("changed somewhere else");
    expect(screen.getByText("Paused")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Resume" })).toBeInTheDocument();
    expect(loads).toBe(2);
  });
});

describe("archive", () => {
  it("asks first, and sends nothing on Cancel", async () => {
    const calls = mockApi({
      ...flowRoutes({ flow: { state: "published", current_version: 2 } }),
      [`POST ${WS}/flows/${FLOW_ID}/archive`]: after("archived"),
    });
    const user = userEvent.setup();
    mount();

    const opener = await screen.findByRole("button", { name: "Archive…" });
    await user.click(opener);
    const dialog = await screen.findByRole("dialog", { name: /^Archive Welcome series\?/ });
    expect(dialog).toHaveTextContent("permanent");
    expect(dialog).toHaveTextContent("cannot be published again");
    expect(dialog).toHaveTextContent("Everyone still in the flow leaves it");
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(opener).toHaveFocus();
    expect(posted(calls, "archive")).toHaveLength(0);
  });

  it("archives on confirmation and leaves the flow read-only", async () => {
    const calls = mockApi({
      ...flowRoutes({ flow: { state: "published", current_version: 2 } }),
      [`POST ${WS}/flows/${FLOW_ID}/archive`]: after("archived"),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Archive…" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Archive flow" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(await screen.findByText("Archived")).toBeInTheDocument();
    expect(screen.getByText(/This flow is archived/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Publish…" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add a step" })).not.toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "Flow name" })).not.toBeInTheDocument();
    expect(posted(calls, "archive")).toHaveLength(1);
  });
});

describe("announcements and notices", () => {
  it("announces the same words again when the flow is paused a second time", async () => {
    let state = "published";
    mockApi({
      ...flowRoutes({ flow: { state: "published", current_version: 2 } }),
      [`POST ${WS}/flows/${FLOW_ID}/pause`]: () => {
        state = "paused";
        return json(flowDetail({ state, current_version: 2 }));
      },
      [`POST ${WS}/flows/${FLOW_ID}/resume`]: () => {
        state = "published";
        return json(flowDetail({ state, current_version: 2 }));
      },
    });
    const user = userEvent.setup();
    mount();
    await screen.findByRole("button", { name: "Pause" });
    const polite = document.querySelector('[aria-live="polite"]')!;
    const spoken: string[] = [];
    const watch = new MutationObserver(() => spoken.push(polite.textContent));
    watch.observe(polite, { childList: true, characterData: true, subtree: true });

    await user.click(await screen.findByRole("button", { name: "Pause" }));
    await screen.findByRole("button", { name: "Resume" });
    await user.click(screen.getByRole("button", { name: "Resume" }));
    await screen.findByRole("button", { name: "Pause" });
    await user.click(screen.getByRole("button", { name: "Pause" }));
    await screen.findByRole("button", { name: "Resume" });

    await waitFor(() => expect(spoken.filter((text) => text === "The flow is paused.")).toHaveLength(2));
    watch.disconnect();
  });

  it("drops an old notice once a later action goes through", async () => {
    let pauses = 0;
    mockApi({
      ...flowRoutes({ flow: { state: "published", current_version: 2 } }),
      [`POST ${WS}/flows/${FLOW_ID}/pause`]: () => {
        pauses += 1;
        return pauses === 1 ? problem(500, "Internal Server Error", "the pause failed") : json(flowDetail({ state: "paused", current_version: 2 }));
      },
      [`POST ${WS}/flows/${FLOW_ID}/publish`]: (request) =>
        new URL(request.url).searchParams.get("dry_run") === "true"
          ? json(impactBody())
          : json(
          {
            flow: flowDetail({ state: "published", current_version: 3 }),
            version: { version: 3, graph: { v: 1, nodes: [], edges: [] }, settings: {}, published_at: "2026-10-20T09:00:00Z", trace_id: "66666666-0000-4000-8000-000000000001" },
          },
          201,
        ),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Pause" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("the pause failed");
    await user.click(screen.getByRole("button", { name: "Publish…" }));
    await user.click(await within(await screen.findByRole("dialog")).findByRole("button", { name: /^Publish version/ }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(screen.queryByText("the pause failed")).not.toBeInTheDocument();
  });

  it("does not archive when the changes could not be saved, and leaves the flow editable", async () => {
    const routes = flowRoutes({ flow: { state: "published", current_version: 2 } });
    const calls = mockApi({
      ...routes,
      [`PATCH ${WS}/flows/${FLOW_ID}`]: () => problem(500, "Internal Server Error", "the save failed"),
      [`POST ${WS}/flows/${FLOW_ID}/archive`]: after("archived"),
    });
    const user = userEvent.setup();
    mount();
    const name = await screen.findByRole("textbox", { name: "Flow name" });
    await user.clear(name);
    await user.type(name, "Renamed{Enter}");

    await user.click(screen.getByRole("button", { name: "Archive…" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Archive flow" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("could not be saved");
    expect(posted(calls, "archive")).toHaveLength(0);
    expect(screen.getByRole("textbox", { name: "Flow name" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Archive…" })).toBeEnabled();
  });
});
