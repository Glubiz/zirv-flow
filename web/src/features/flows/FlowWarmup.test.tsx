import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { FLOW_ID, flowDetail, flowRoutes, impactBody, warmupBody, WS } from "../../test/flowsApi";
import { mountEditor as mount } from "../../test/flowsEditor";
import { json, mockApi, problem } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

beforeEach(() => {
  stubReactFlow();
});

/** A window of five minutes that started `elapsed` seconds ago. */
function running(elapsed: number, overrides: Record<string, unknown> = {}) {
  const started = Date.now() - elapsed * 1000;
  return warmupBody({
    started_at: new Date(started).toISOString(),
    ends_at: new Date(started + 300_000).toISOString(),
    admitted: 12,
    waiting: 30,
    ...overrides,
  });
}

const LIVE = { state: "published", current_version: 2 };

describe("the warm-up state line", () => {
  it("shows how far along the window is, how long is left and who is in and waiting", async () => {
    mockApi(flowRoutes({ flow: { ...LIVE, warmup: running(120) } }));
    mount();

    expect(await screen.findByText("Warming up · 40% · 3 min left")).toBeInTheDocument();
    expect(screen.getByText("12 admitted · 30 waiting")).toBeInTheDocument();
  });

  it("appears after the first publish without a reload", async () => {
    let published = false;
    mockApi({
      ...flowRoutes({ flow: { state: "draft", current_version: null } }),
      [`GET ${WS}/flows/${FLOW_ID}`]: () =>
        json(flowDetail(published ? { ...LIVE, warmup: running(1) } : { state: "draft", current_version: null })),
      [`POST ${WS}/flows/${FLOW_ID}/publish`]: (request) => {
        if (new URL(request.url).searchParams.get("dry_run") === "true") return json(impactBody());
        published = true;
        return json(
          {
            flow: flowDetail({ ...LIVE, warmup: warmupBody() }),
            version: { version: 2, graph: { v: 1, nodes: [], edges: [] }, settings: {}, published_at: "2026-10-20T09:00:00Z", trace_id: "66666666-0000-4000-8000-000000000001" },
          },
          201,
        );
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /^Review and publish/ }));
    await user.click(await within(await screen.findByRole("dialog")).findByRole("button", { name: /^Publish version/ }));

    expect(await screen.findByText(/^Warming up/)).toBeInTheDocument();
  });

  it("is not shown when the window is over, off, or opened", async () => {
    for (const warmup of [
      warmupBody(),
      running(400, { ends_at: new Date(Date.now() - 100_000).toISOString() }),
      running(120, { enabled: false }),
      running(120, { opened_at: new Date().toISOString() }),
    ]) {
      mockApi(flowRoutes({ flow: { ...LIVE, warmup } }));
      const view = mount();
      await screen.findByRole("group", { name: /^Wait: 1 day/ });
      expect(screen.queryByText(/^Warming up/)).not.toBeInTheDocument();
      view.unmount();
    }
  });

  it("lets an admin open the flow now, after asking", async () => {
    const calls = mockApi({
      ...flowRoutes({ flow: { ...LIVE, warmup: running(120) } }),
      [`POST ${WS}/flows/${FLOW_ID}/warmup/open`]: () =>
        json(flowDetail({ ...LIVE, warmup: running(121, { opened_at: new Date().toISOString(), waiting: 0, admitted: 42 }) })),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Open now" }));
    const dialog = screen.getByRole("dialog", { name: "Open the flow now?" });
    expect(calls.filter((call) => call.path === `${WS}/flows/${FLOW_ID}/warmup/open`)).toHaveLength(0);
    await user.click(within(dialog).getByRole("button", { name: "Open now" }));

    await waitFor(() => expect(screen.queryByText(/^Warming up/)).not.toBeInTheDocument());
    expect(calls.filter((call) => call.method === "POST" && call.path === `${WS}/flows/${FLOW_ID}/warmup/open`)).toHaveLength(1);
  });

  it("offers a publisher the line but not the action", async () => {
    mockApi(flowRoutes({ role: "publisher", flow: { ...LIVE, warmup: running(120) } }));
    mount();

    expect(await screen.findByText("Warming up · 40% · 3 min left")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Open now" })).not.toBeInTheDocument();
  });

  it("says why when the flow could not be opened", async () => {
    mockApi({
      ...flowRoutes({ flow: { ...LIVE, warmup: running(120) } }),
      [`POST ${WS}/flows/${FLOW_ID}/warmup/open`]: () => problem(403, "Forbidden", "you cannot open this flow"),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Open now" }));
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Open now" }));

    expect(await screen.findByText("you cannot open this flow")).toBeInTheDocument();
    expect(screen.getByText(/^Warming up/)).toBeInTheDocument();
  });
});

describe("the warm-up setting", () => {
  function settings() {
    return screen.getByRole("region", { name: "Flow settings" });
  }

  it("is on with five minutes for a flow that never changed it", async () => {
    mockApi(flowRoutes({ role: "publisher" }));
    mount();

    const section = within(await screen.findByRole("region", { name: "Flow settings" }));
    expect(section.getByRole("checkbox", { name: "Warm up when the flow starts" })).toBeChecked();
    expect(section.getByRole("spinbutton", { name: "Amount" })).toHaveValue(5);
    expect(section.getByRole("combobox", { name: "Unit" })).toHaveValue("minute");
    expect(section.queryByRole("button", { name: "Save warm-up" })).not.toBeInTheDocument();
  });

  it("saves a new length on its own, without a publish", async () => {
    const calls = mockApi({
      ...flowRoutes({ role: "publisher", flow: LIVE }),
      [`PUT ${WS}/flows/${FLOW_ID}/warmup`]: async (request) => {
        const body = (await request.json()) as { enabled: boolean; seconds: number };
        return json(flowDetail({ ...LIVE, warmup: warmupBody(body) }));
      },
    });
    const user = userEvent.setup();
    mount();

    const amount = await within(await screen.findByRole("region", { name: "Flow settings" })).findByRole("spinbutton", { name: "Amount" });
    await user.clear(amount);
    await user.type(amount, "2");
    await user.selectOptions(within(settings()).getByRole("combobox", { name: "Unit" }), "hour");
    await user.click(within(settings()).getByRole("button", { name: "Save warm-up" }));

    await waitFor(() => expect(within(settings()).queryByRole("button", { name: "Save warm-up" })).not.toBeInTheDocument());
    const puts = calls.filter((call) => call.method === "PUT" && call.path === `${WS}/flows/${FLOW_ID}/warmup`);
    expect(puts).toHaveLength(1);
    expect(puts[0]?.body).toEqual({ enabled: true, seconds: 7200 });
    expect(calls.filter((call) => call.path.endsWith("/publish"))).toHaveLength(0);
  });

  it("turns it off", async () => {
    const calls = mockApi({
      ...flowRoutes({ role: "publisher", flow: LIVE }),
      [`PUT ${WS}/flows/${FLOW_ID}/warmup`]: async (request) => {
        const body = (await request.json()) as { enabled: boolean; seconds: number };
        return json(flowDetail({ ...LIVE, warmup: warmupBody(body) }));
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await within(await screen.findByRole("region", { name: "Flow settings" })).findByRole("checkbox", { name: "Warm up when the flow starts" }));
    await user.click(within(settings()).getByRole("button", { name: "Save warm-up" }));

    await waitFor(() => expect(within(settings()).getByRole("checkbox", { name: "Warm up when the flow starts" })).not.toBeChecked());
    const puts = calls.filter((call) => call.method === "PUT");
    expect(puts[0]?.body).toEqual({ enabled: false, seconds: 300 });
    expect(within(settings()).queryByRole("spinbutton", { name: "Amount" })).not.toBeInTheDocument();
  });

  it("keeps the length between a minute and a day", async () => {
    mockApi(flowRoutes({ role: "publisher", flow: LIVE }));
    const user = userEvent.setup();
    mount();

    const amount = await within(await screen.findByRole("region", { name: "Flow settings" })).findByRole("spinbutton", { name: "Amount" });
    await user.clear(amount);
    await user.type(amount, "30");
    await user.selectOptions(within(settings()).getByRole("combobox", { name: "Unit" }), "second");

    expect(within(settings()).getByText("Between 1 minute and 24 hours.")).toBeInTheDocument();
    expect(within(settings()).getByRole("button", { name: "Save warm-up" })).toBeDisabled();
    await user.selectOptions(within(settings()).getByRole("combobox", { name: "Unit" }), "day");
    await user.clear(amount);
    await user.type(amount, "2");
    expect(within(settings()).getByText("Between 1 minute and 24 hours.")).toBeInTheDocument();
    await user.clear(amount);
    await user.type(amount, "1");
    expect(within(settings()).queryByText("Between 1 minute and 24 hours.")).not.toBeInTheDocument();
    expect(within(settings()).getByRole("button", { name: "Save warm-up" })).toBeEnabled();
  });

  it("is read-only for someone who cannot publish", async () => {
    mockApi(flowRoutes({ role: "editor" }));
    mount();

    const section = within(await screen.findByRole("region", { name: "Flow settings" }));
    expect(section.getByRole("checkbox", { name: "Warm up when the flow starts" })).toBeDisabled();
    expect(section.getByRole("spinbutton", { name: "Amount" })).toBeDisabled();
  });
});
