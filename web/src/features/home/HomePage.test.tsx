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

function routes(overrides: Record<string, Handler> = {}, role = "admin") {
  const who = role === "admin" ? OWNER : MEMBER;
  return {
    "GET /api/v1/session": () => json(session(who)),
    "GET /api/v1/workspaces": () => json([WORKSPACE]),
    "GET /api/v1/workspaces/ws1/members": () => json([{ user_id: who.id, email: who.email, role, added_at: "2026-10-01T09:00:00Z" }]),
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
    expect(await screen.findByRole("status")).toBeInTheDocument();
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

  it("does not request admin-only lists or claim setup is complete for a viewer", async () => {
    const calls = mockApi(routes({}, "viewer"));
    home();
    expect(await screen.findByText("No running flows")).toBeInTheDocument();
    expect(screen.getByText("Sending domain checks need admin access")).toBeInTheDocument();
    expect(screen.queryByRole("progressbar")).not.toBeInTheDocument();
    expect(calls.some((call) => /sending-domains|destinations/.test(call.path))).toBe(false);
  });
});
