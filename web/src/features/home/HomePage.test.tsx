import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session, WORKSPACE, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { WorkspacesProvider } from "../workspaces";
import { setupChecklist, type SetupFacts } from "./checklist";
import { HomePage, SetupChecklist } from "./HomePage";

const EMPTY: SetupFacts = { sources: [], domains: [], destinations: [], templates: [], segments: [], flows: [] };
const COMPLETE: SetupFacts = {
  sources: [{ id: "s1" }], domains: [{ status: "verified" }], destinations: [{ archived: false }],
  templates: [{}], segments: [{}], flows: [{ state: "published", current_version: 1 }],
};

const SUMMARY = {
  window: { from: "2026-10-24T00:00:00Z", to: "2026-10-31T00:00:00Z" },
  messages_sent: 6945, goals_reached: 642, people_created: 1120, events_received: 2_400_000,
  flows: [], computed_at: "2026-10-31T00:00:00Z",
};

function routes(overrides: Record<string, Handler> = {}, role = "admin") {
  const who = role === "admin" ? OWNER : MEMBER;
  return {
    "GET /api/v1/session": () => json(session(who)),
    "GET /api/v1/workspaces": () => json([WORKSPACE]),
    "GET /api/v1/workspaces/ws1/members": () => json([{ user_id: who.id, email: who.email, role, added_at: "2026-10-01T09:00:00Z" }]),
    "GET /api/v1/workspaces/ws1/summary": () => json(SUMMARY),
    ...Object.fromEntries(["sources", "flows", "sending-domains", "destinations", "templates", "segments"].map((path) => [`GET /api/v1/workspaces/ws1/${path}`, () => json([])] as const)),
    ...overrides,
  };
}

function home() {
  return renderRoute("/w/ws1", "/w/:ws", <WorkspacesProvider><HomePage /></WorkspacesProvider>);
}

describe("setup checklist", () => {
  it("derives each step in setup order, including incomplete and complete workspaces", () => {
    const steps = setupChecklist(EMPTY);
    expect(steps.map((step) => step.route)).toEqual(["sources", "sending-domains", "destinations", "templates", "segments", "flows", "flows"]);
    expect(steps.every((step) => !step.done)).toBe(true);
    expect(setupChecklist(COMPLETE).every((step) => step.done)).toBe(true);
    expect(setupChecklist({ ...COMPLETE, domains: [{ status: "unverified" }], flows: [{ state: "draft" }] }).map((step) => step.done)).toEqual([true, false, true, true, true, true, false]);
  });

  it("does not count archived objects and does count a previously published paused flow", () => {
    expect(setupChecklist({ ...EMPTY, flows: [{ state: "paused", current_version: 1 }] }).slice(-2).every((step) => step.done)).toBe(true);
    expect(setupChecklist({ ...EMPTY, destinations: [{ archived: true }], templates: [{ archived_at: "now" }], segments: [{ archived_at: "now" }], flows: [{ state: "archived", current_version: 1 }] }).every((step) => !step.done)).toBe(true);
  });

  it("links incomplete steps and disappears when all steps are done", () => {
    const view = render(<MemoryRouter><SetupChecklist base="/w/ws1" data={EMPTY} /></MemoryRouter>);
    expect(screen.getByRole("link", { name: "Verify a sending domain: Not done" })).toHaveAttribute("href", "/w/ws1/sending-domains");
    expect(screen.getByRole("progressbar", { name: "Workspace setup" })).toHaveAttribute("value", "0");
    view.rerender(<MemoryRouter><SetupChecklist base="/w/ws1" data={COMPLETE} /></MemoryRouter>);
    expect(screen.queryByRole("region", { name: "Set up your workspace" })).not.toBeInTheDocument();
  });
});

describe("Home", () => {
  it("shows loading and empty states without invented totals", async () => {
    mockApi(routes());
    home();
    expect((await screen.findAllByRole("status")).length).toBeGreaterThan(0);
    expect(await screen.findByText("No running flows")).toBeInTheDocument();
    expect(screen.getByText("No sources yet")).toBeInTheDocument();
    expect(screen.getByText("No sending domains need attention")).toBeInTheDocument();
    expect(screen.getByText("0 of 7 complete")).toBeInTheDocument();
  });

  it("shows only live flows and endpoint-backed sending and source states", async () => {
    mockApi(routes({
      "GET /api/v1/workspaces/ws1/flows": () => json([{ id: "f1", name: "Welcome series", state: "published", current_version: 1 }, { id: "f2", name: "Paused series", state: "paused" }]),
      "GET /api/v1/workspaces/ws1/sources": () => json([{ id: "s1", name: "Shop webhook", enabled: true }]),
      "GET /api/v1/workspaces/ws1/sending-domains": () => json([{ id: "d1", domain: "mail.example.test", status: "unverified" }]),
    }));
    home();
    expect(await screen.findByRole("link", { name: "Welcome series" })).toHaveAttribute("href", "/w/ws1/flows/f1");
    expect(screen.queryByText("Paused series")).not.toBeInTheDocument();
    expect(screen.getByRole("link", { name: "mail.example.test" })).toHaveAttribute("href", "/w/ws1/sending-domains/d1");
    expect(within(screen.getByRole("region", { name: "Data coming in" })).getByText("Enabled")).toBeInTheDocument();
  });

  it("reports a list failure and retries without treating it as an empty workspace", async () => {
    let failed = true;
    mockApi(routes({ "GET /api/v1/workspaces/ws1/sources": () => failed ? problem(503, "Unavailable", "Sources are unavailable") : json([]) }));
    const user = userEvent.setup();
    home();
    expect(await screen.findByRole("alert")).toHaveTextContent("Sources are unavailable");
    expect(screen.queryByText("No sources yet")).not.toBeInTheDocument();
    failed = false;
    await user.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByText("No sources yet")).toBeInTheDocument();
  });

  it("shows the four figures with compact numbers, links and accessible names", async () => {
    mockApi(routes());
    home();
    const tiles = await screen.findByRole("region", { name: "Last 7 days" });
    expect(within(tiles).getByRole("link", { name: "Messages sent, 6,945, last 7 days" })).toHaveAttribute("href", "/w/ws1/flows");
    expect(within(tiles).getByRole("link", { name: "Goals reached, 642, last 7 days" })).toHaveAttribute("href", "/w/ws1/flows");
    expect(within(tiles).getByRole("link", { name: "New people, 1,120, last 7 days" })).toHaveAttribute("href", "/w/ws1/people");
    expect(within(tiles).getByRole("link", { name: "Events received, 2.4 M, last 7 days" })).toHaveAttribute("href", "/w/ws1/events");
  });

  it("shows zero as a figure and No access when events are null", async () => {
    mockApi(routes({ "GET /api/v1/workspaces/ws1/summary": () => json({ ...SUMMARY, messages_sent: 0, events_received: null }) }));
    home();
    const tiles = await screen.findByRole("region", { name: "Last 7 days" });
    expect(within(tiles).getByRole("link", { name: "Messages sent, 0, last 7 days" })).toBeInTheDocument();
    expect(within(tiles).getByRole("link", { name: "Events received, No access, last 7 days" })).toHaveTextContent("No access");
  });

  it("keeps the rest of Home when the figures fail, and retries them", async () => {
    let failed = true;
    mockApi(routes({ "GET /api/v1/workspaces/ws1/summary": () => failed ? problem(503, "Unavailable", "Figures are unavailable") : json(SUMMARY) }));
    const user = userEvent.setup();
    home();
    const tiles = await screen.findByRole("region", { name: "Last 7 days" });
    expect(await within(tiles).findByRole("alert")).toHaveTextContent("Could not load figures");
    expect(screen.getByText("No sources yet")).toBeInTheDocument();
    expect(within(tiles).queryByRole("link")).not.toBeInTheDocument();
    failed = false;
    await user.click(within(tiles).getByRole("button", { name: "Try again" }));
    expect(await within(tiles).findByRole("link", { name: "Messages sent, 6,945, last 7 days" })).toBeInTheDocument();
  });

  it("shows the figures loading in the tile row only", async () => {
    mockApi(routes({ "GET /api/v1/workspaces/ws1/summary": () => new Promise<Response>(() => {}) }));
    home();
    const tiles = await screen.findByRole("region", { name: "Last 7 days" });
    expect(within(tiles).getByRole("status")).toHaveTextContent("Loading figures");
    expect(screen.getByText("No sources yet")).toBeInTheDocument();
  });

  it("lists running flows with their figures and a dash for no goal", async () => {
    mockApi(routes({
      "GET /api/v1/workspaces/ws1/flows": () => json([{ id: "f1", name: "Welcome series", state: "published", current_version: 1 }, { id: "f2", name: "Newsletter", state: "published", current_version: 1 }]),
      "GET /api/v1/workspaces/ws1/summary": () => json({ ...SUMMARY, flows: [
        { flow_id: "f1", people_in_flow: 1284, sent: 3902, goals_reached: 642 },
        { flow_id: "f2", people_in_flow: 12, sent: 0, goals_reached: null },
      ] }),
    }));
    home();
    const table = await screen.findByRole("table", { name: "Running flows" });
    expect(within(table).getAllByRole("columnheader").map((header) => header.textContent)).toEqual(["Flow", "In flow", "Sent", "Goal reached"]);
    const rows = within(table).getAllByRole("row").slice(1).map((row) => within(row).getAllByRole("cell").map((cell) => cell.textContent));
    expect(rows[0]?.slice(1)).toEqual(["1,284", "3,902", "642"]);
    expect(rows[1]?.slice(1)).toEqual(["12", "0", "—"]);
  });

  it("does not request admin-only lists or claim setup is complete for a viewer", async () => {
    const calls = mockApi(routes({}, "viewer"));
    home();
    expect(await screen.findByText("No running flows")).toBeInTheDocument();
    expect(screen.getByText("Sending domain checks need admin access")).toBeInTheDocument();
    expect(screen.queryByRole("progressbar")).not.toBeInTheDocument();
    expect(calls.some((call) => /sending-domains|destinations/.test(call.path))).toBe(false);
  });
});
