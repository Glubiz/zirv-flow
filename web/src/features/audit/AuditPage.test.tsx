import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session, WORKSPACE, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { WorkspacesProvider } from "../workspaces";
import { AuditNav } from "./AuditNav";
import { AuditPage } from "./AuditPage";

const TRACE = "0123456789abcdef0123456789abcdef";

function entry(id: number, overrides: Record<string, unknown> = {}) {
  return {
    id,
    action: "membership.set",
    actor_kind: "user",
    actor_id: "11111111-2222-3333-4444-555555555555",
    object_kind: "user",
    object_id: "66666666-7777-8888-9999-000000000000",
    occurred_at: "2026-10-27T09:30:00Z",
    request_id: `req-${id}-aaaaaaaaaaaa`,
    summary: { role: { from: "editor", to: "publisher" }, scopes: ["read", "write"] },
    trace_id: TRACE,
    workspace_id: "ws1",
    ...overrides,
  };
}

const CATALOGUE = {
  areas: [
    { area: "flow", actions: ["flow.created", "flow.published"] },
    { area: "membership", actions: ["membership.set", "membership.removed"] },
    { area: "session", actions: ["session.signed_in"] },
    { area: "brand_new", actions: ["brand_new.thing"] },
  ],
};

function members(role: string) {
  return [{ user_id: MEMBER.id, email: MEMBER.email, role, added_at: "2026-10-02T09:00:00Z" }];
}

function routes(extra: Record<string, Handler>, who: typeof OWNER = OWNER, role = "admin") {
  return {
    "GET /api/v1/session": () => json(session(who)),
    "GET /api/v1/workspaces": () => json([WORKSPACE]),
    "GET /api/v1/workspaces/ws1/members": () => json(members(role)),
    "GET /api/v1/workspaces/ws1/audit/actions": () => json(CATALOGUE),
    ...extra,
  };
}

function page(path = "/w/ws1/settings/audit") {
  return renderRoute(
    path,
    "/w/:ws/settings/audit",
    <WorkspacesProvider>
      <AuditPage />
    </WorkspacesProvider>,
  );
}

afterEach(() => vi.restoreAllMocks());

describe("audit log", () => {
  it("renders the rows of a page with a readable action and its code", async () => {
    mockApi(
      routes({
        "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [entry(2), entry(1, { action: "flow.published" })], next_cursor: null }),
      }),
    );
    page();

    const table = await screen.findByRole("table");
    expect(within(table).getByText("Membership set")).toBeInTheDocument();
    expect(within(table).getByText("membership.set")).toBeInTheDocument();
    expect(within(table).getByText("Flow published")).toBeInTheDocument();
    expect(within(table).getAllByRole("row")).toHaveLength(3);
    expect(screen.queryByRole("button", { name: "Load more" })).not.toBeInTheDocument();
  });

  it("appends the next page on Load more and stops at the last page", async () => {
    const calls = mockApi(
      routes({
        "GET /api/v1/workspaces/ws1/audit": (request) =>
          new URL(request.url).searchParams.get("cursor") === "c2"
            ? json({ entries: [entry(1, { action: "flow.created" })], next_cursor: null })
            : json({ entries: [entry(2)], next_cursor: "c2" }),
      }),
    );
    page();

    await screen.findByText("Membership set");
    await userEvent.click(screen.getByRole("button", { name: "Load more" }));
    expect(await screen.findByText("Flow created")).toBeInTheDocument();
    expect(screen.getByText("Membership set")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Load more" })).not.toBeInTheDocument();
    expect(calls.filter((call) => call.path.endsWith("/audit")).map((call) => call.search)).toEqual(["", "?cursor=c2"]);
  });

  it("sends each filter in the request", async () => {
    const calls = mockApi(routes({ "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [entry(1)], next_cursor: null }) }));
    page();
    await screen.findByText("Membership set");
    const user = userEvent.setup();

    await user.selectOptions(screen.getByLabelText("Action"), "flow.published");
    await user.selectOptions(screen.getByLabelText("Actor kind"), "api_key");
    await user.type(screen.getByLabelText("Actor id"), "actor-1");
    await user.type(screen.getByLabelText("Object kind"), "flow");
    await user.type(screen.getByLabelText("Object id"), "obj-1");
    await user.type(screen.getByLabelText("From"), "2026-10-01");
    await user.type(screen.getByLabelText("To"), "2026-10-02");
    await user.type(screen.getByLabelText("Request id"), "req-9");
    await user.click(screen.getByRole("button", { name: "Apply" }));

    await waitFor(() => {
      const last = calls.filter((call) => call.path.endsWith("/audit")).at(-1);
      const params = new URLSearchParams(last?.search);
      expect(params.get("action")).toBe("flow.published");
      expect(params.get("actor_kind")).toBe("api_key");
      expect(params.get("actor_id")).toBe("actor-1");
      expect(params.get("object_kind")).toBe("flow");
      expect(params.get("object_id")).toBe("obj-1");
      expect(params.get("from")).toBe(new Date(2026, 9, 1).toISOString());
      expect(params.get("to")).toBe(new Date(2026, 9, 3).toISOString());
      expect(params.get("request_id")).toBe("req-9");
    });
  });

  it("offers an area prefix, groups the catalogue and keeps an unmapped area under Other", async () => {
    mockApi(routes({ "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [], next_cursor: null }) }));
    page();
    const select = await screen.findByLabelText("Action");
    await waitFor(() => expect(within(select).getByRole("option", { name: "Any flow action" })).toBeInTheDocument());

    const groups = Array.from(select.querySelectorAll("optgroup")).map((group) => group.label);
    expect(groups).toEqual(["Sign-in", "Members and keys", "Publishing", "Other"]);
    expect(within(select).getByRole("option", { name: "Brand new thing (brand_new.thing)" })).toBeInTheDocument();
  });

  it("round-trips the filters through the URL", async () => {
    const calls = mockApi(routes({ "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [entry(1)], next_cursor: null }) }));
    const { router } = page("/w/ws1/settings/audit?action=flow.&request_id=req-7&from=2026-10-05");

    expect(await screen.findByLabelText("Request id")).toHaveValue("req-7");
    expect(screen.getByLabelText("From")).toHaveValue("2026-10-05");
    expect(screen.getByLabelText("Action")).toHaveValue("flow.");
    await waitFor(() => expect(calls.some((call) => call.search.includes("request_id=req-7"))).toBe(true));

    await userEvent.clear(screen.getByLabelText("Request id"));
    await userEvent.type(screen.getByLabelText("Request id"), "req-8");
    await userEvent.click(screen.getByRole("button", { name: "Apply" }));

    await waitFor(() => expect(router.state.location.search).toContain("request_id=req-8"));
    expect(router.state.location.search).toContain("action=flow.");
    expect(router.state.location.search).not.toContain("req-7");
  });

  it("opens the entry with its changes and the trace link", async () => {
    mockApi(routes({ "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [entry(1)], next_cursor: null }) }));
    page();

    await userEvent.click(await screen.findByRole("button", { name: "Open entry membership.set" }));

    const sheet = await screen.findByRole("dialog", { name: "Membership set" });
    expect(within(sheet).getByText("Changes")).toBeInTheDocument();
    expect(within(sheet).getByText("editor -> publisher")).toBeInTheDocument();
    expect(within(sheet).getByText("read, write")).toBeInTheDocument();
    expect(sheet.textContent).not.toContain("{");
    expect(within(sheet).getByRole("link", { name: "Open trace" })).toHaveAttribute("href", `/w/ws1/traces/${TRACE}`);
    expect(within(sheet).getByRole("button", { name: "Copy request id" })).toBeInTheDocument();
  });

  it("shows no trace link for an entry without a trace id", async () => {
    mockApi(
      routes({
        "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [entry(1, { trace_id: null })], next_cursor: null }),
      }),
    );
    page();

    await userEvent.click(await screen.findByRole("button", { name: "Open entry membership.set" }));

    const sheet = await screen.findByRole("dialog", { name: "Membership set" });
    expect(within(sheet).queryByRole("link", { name: "Open trace" })).not.toBeInTheDocument();
  });

  it("ignores a late Load more of an older filter: the new one stays disabled until it resolves and nothing is appended twice", async () => {
    let releaseA: () => void = () => undefined;
    let releaseB: () => void = () => undefined;
    const gateA = new Promise<void>((resolve) => {
      releaseA = resolve;
    });
    const gateB = new Promise<void>((resolve) => {
      releaseB = resolve;
    });
    mockApi(
      routes({
        "GET /api/v1/workspaces/ws1/audit": async (request) => {
          const params = new URL(request.url).searchParams;
          const filtered = params.get("action") === "flow.created";
          const cursor = params.get("cursor");
          if (!filtered) {
            if (cursor === null) return json({ entries: [entry(10)], next_cursor: "a2" });
            await gateA;
            return json({ entries: [entry(11, { action: "session.signed_in" })], next_cursor: null });
          }
          if (cursor === null) return json({ entries: [entry(20, { action: "flow.created" })], next_cursor: "b2" });
          await gateB;
          return json({ entries: [entry(21, { action: "flow.published" })], next_cursor: null });
        },
      }),
    );
    page();
    const user = userEvent.setup();

    await screen.findByText("Membership set");
    await user.click(screen.getByRole("button", { name: "Load more" }));
    await screen.findByRole("button", { name: "Loading" });

    await waitFor(() => expect(within(screen.getByLabelText("Action")).getByRole("option", { name: "Flow created (flow.created)" })).toBeInTheDocument());
    await user.selectOptions(screen.getByLabelText("Action"), "flow.created");
    await user.click(screen.getByRole("button", { name: "Apply" }));
    await screen.findByText("Flow created");
    await user.click(screen.getByRole("button", { name: "Load more" }));
    expect(screen.getByRole("button", { name: "Loading" })).toBeDisabled();

    releaseA();
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(screen.getByRole("button", { name: "Loading" })).toBeDisabled();
    expect(screen.queryByText("Session signed in")).not.toBeInTheDocument();

    releaseB();
    expect(await screen.findByText("Flow published")).toBeInTheDocument();
    expect(screen.getAllByText("Flow published")).toHaveLength(1);
    expect(screen.queryByRole("button", { name: "Load more" })).not.toBeInTheDocument();
    expect(screen.queryByText("Session signed in")).not.toBeInTheDocument();
  });

  it("shows a failed action catalogue in the Action field and a retry that loads it", async () => {
    let fail = true;
    mockApi(
      routes({
        "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [entry(1)], next_cursor: null }),
        "GET /api/v1/workspaces/ws1/audit/actions": () => (fail ? problem(500, "Server error", "No actions today", "t-2") : json(CATALOGUE)),
      }),
    );
    page();

    expect(await screen.findByText("No actions today")).toBeInTheDocument();
    fail = false;
    await userEvent.click(screen.getByRole("button", { name: "Retry actions" }));
    await waitFor(() => expect(within(screen.getByLabelText("Action")).getByRole("option", { name: "Any flow action" })).toBeInTheDocument());
    expect(screen.queryByText("No actions today")).not.toBeInTheDocument();
  });

  it("says in the Action field that the catalogue is loading, and that it is empty", async () => {
    let release: () => void = () => undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    mockApi(
      routes({
        "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [entry(1)], next_cursor: null }),
        "GET /api/v1/workspaces/ws1/audit/actions": async () => {
          await gate;
          return json({ areas: [] });
        },
      }),
    );
    page();

    expect(await screen.findByText("Loading the actions")).toBeInTheDocument();
    expect(screen.getByLabelText("Action")).toBeDisabled();
    release();
    expect(await screen.findByText("No actions are recorded yet")).toBeInTheDocument();
  });

  it("keeps the bar to Action, Actor, Object, From, To and Apply, with the rest behind More filters", async () => {
    mockApi(routes({ "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [entry(1)], next_cursor: null }) }));
    page();
    await screen.findByText("Membership set");

    const form = screen.getByRole("form", { name: "Filter the audit log" });
    expect(within(form).queryByLabelText("Actor id")).not.toBeInTheDocument();
    await userEvent.selectOptions(within(form).getByLabelText("Actor kind"), "user");
    expect(within(form).getByLabelText("Actor id")).toBeInTheDocument();
    const more = form.querySelector("details");
    expect(more).not.toBeNull();
    expect(within(more as HTMLElement).getByText("More filters")).toBeInTheDocument();
    expect(within(more as HTMLElement).getByLabelText("Object kind")).toBeInTheDocument();
    expect(within(more as HTMLElement).getByLabelText("Request id")).toBeInTheDocument();
    expect(form.querySelector(".audit-more")?.contains(within(form).getByLabelText("Object id"))).toBe(false);
    expect(screen.getByRole("button", { name: "Filters" })).toHaveAttribute("aria-expanded", "false");
  });

  it("shows a user actor as the user icon and links an archived flow", async () => {
    mockApi(
      routes({
        "GET /api/v1/workspaces/ws1/audit": () =>
          json({ entries: [entry(1, { action: "flow.archived", object_kind: "flow", object_id: "flow-1" })], next_cursor: null }),
      }),
    );
    page();

    const table = await screen.findByRole("table");
    expect(within(table).getByRole("img", { name: "User" })).toBeInTheDocument();
    expect(within(table).queryByText("User")).not.toBeInTheDocument();
    await userEvent.click(within(table).getByRole("button", { name: "Open entry flow.archived" }));
    const sheet = await screen.findByRole("dialog", { name: "Flow archived" });
    expect(within(sheet).getByRole("link", { name: "flow-1" })).toHaveAttribute("href", "/w/ws1/flows/flow-1");
  });

  it("closes the entry sheet with Escape and with its button, and returns focus to the row", async () => {
    mockApi(routes({ "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [entry(1)], next_cursor: null }) }));
    page();
    const user = userEvent.setup();

    const row = await screen.findByRole("button", { name: "Open entry membership.set" });
    await user.click(row);
    const sheet = await screen.findByRole("dialog", { name: "Membership set" });
    expect(sheet).toHaveFocus();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(row).toHaveFocus();

    await user.click(row);
    await user.click(await screen.findByRole("button", { name: "Close entry" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(row).toHaveFocus();
  });

  it("holds the entry sheet modal: Tab stays inside, the page behind is inert, and Escape and the backdrop close it", async () => {
    mockApi(routes({ "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [entry(1)], next_cursor: null }) }));
    const view = page();
    const user = userEvent.setup();

    const row = await screen.findByRole("button", { name: "Open entry membership.set" });
    await user.click(row);
    const sheet = await screen.findByRole("dialog", { name: "Membership set" });
    expect(sheet).toHaveAttribute("aria-modal", "true");
    expect(view.container).toHaveAttribute("inert");

    const link = within(sheet).getByRole("link", { name: "Open trace" });
    link.focus();
    await user.tab();
    expect(sheet.contains(document.activeElement)).toBe(true);
    await user.tab({ shift: true });
    await user.tab({ shift: true });
    expect(sheet.contains(document.activeElement)).toBe(true);

    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(view.container).not.toHaveAttribute("inert");
    expect(row).toHaveFocus();

    await user.click(row);
    await screen.findByRole("dialog");
    await user.click(document.querySelector(".sheet-backdrop") as HTMLElement);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(row).toHaveFocus();
  });

  it("shows a loading state, then the empty state", async () => {
    let release: () => void = () => undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    mockApi(
      routes({
        "GET /api/v1/workspaces/ws1/audit": async () => {
          await gate;
          return json({ entries: [], next_cursor: null });
        },
      }),
    );
    page();

    expect(await screen.findByText("Loading the audit log")).toBeInTheDocument();
    release();
    expect(await screen.findByText("No entries match these filters")).toBeInTheDocument();
  });

  it("shows an error with a retry that reads again", async () => {
    let fail = true;
    mockApi(
      routes({
        "GET /api/v1/workspaces/ws1/audit": () =>
          fail ? problem(500, "Server error", "It broke", "t-1") : json({ entries: [entry(1)], next_cursor: null }),
      }),
    );
    page();

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("It broke");
    fail = false;
    await userEvent.click(within(alert).getByRole("button", { name: "Try again" }));
    expect(await screen.findByText("Membership set")).toBeInTheDocument();
  });

  it("builds the export URL for both formats with the current filters", async () => {
    mockApi(routes({ "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [entry(1)], next_cursor: null }) }));
    const clicked: string[] = [];
    vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (this: HTMLAnchorElement) {
      clicked.push(new URL(this.href).pathname + new URL(this.href).search);
    });
    page("/w/ws1/settings/audit?action=flow.&request_id=req-7");
    await screen.findByText("Membership set");
    const user = userEvent.setup();

    await user.click(screen.getByRole("button", { name: "Export" }));
    await user.click(screen.getByRole("menuitem", { name: "NDJSON" }));
    await user.click(screen.getByRole("button", { name: "Export" }));
    await user.click(screen.getByRole("menuitem", { name: "CSV" }));

    expect(clicked).toEqual([
      "/api/v1/workspaces/ws1/audit/export?format=ndjson&action=flow.&request_id=req-7",
      "/api/v1/workspaces/ws1/audit/export?format=csv&action=flow.&request_id=req-7",
    ]);
    expect(screen.getByText("Exporting is itself recorded in this log.")).toBeInTheDocument();
  });

  it("shows the Instance switch to the owner and reads the instance rows", async () => {
    const calls = mockApi(
      routes({
        "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [entry(1)], next_cursor: null }),
        "GET /api/v1/audit": () => json({ entries: [entry(9, { action: "session.signed_in", workspace_id: null })], next_cursor: null }),
      }),
    );
    page();

    await userEvent.click(await screen.findByRole("switch", { name: "Instance" }));

    expect(await screen.findByText("Session signed in")).toBeInTheDocument();
    expect(calls.some((call) => call.path === "/api/v1/audit")).toBe(true);
  });

  it("does not show the Instance switch to a workspace admin who is not the owner", async () => {
    mockApi(
      routes({ "GET /api/v1/workspaces/ws1/audit": () => json({ entries: [entry(1)], next_cursor: null }) }, MEMBER, "admin"),
    );
    page();

    await screen.findByText("Membership set");
    expect(screen.queryByRole("switch", { name: "Instance" })).not.toBeInTheDocument();
  });

  it.each(["editor", "viewer"])("sends %s away from the page and hides the menu entry", async (role) => {
    const calls = mockApi(routes({}, MEMBER, role));
    const { router } = page();

    await waitFor(() => expect(router.state.location.pathname).toBe("/w/ws1/members"));
    expect(calls.some((call) => call.path.endsWith("/audit"))).toBe(false);

    const nav = renderRoute(
      "/w/ws1/members",
      "/w/:ws/members",
      <WorkspacesProvider>
        <AuditNav workspaceId="ws1" />
        <p>nav rendered</p>
      </WorkspacesProvider>,
    );
    expect(await screen.findAllByText("nav rendered")).not.toHaveLength(0);
    expect(screen.queryByRole("link", { name: "Audit log" })).not.toBeInTheDocument();
    nav.unmount();
  });

  it("shows the menu entry to an admin", async () => {
    mockApi(routes({}, MEMBER, "admin"));
    renderRoute("/w/ws1/members", "/w/:ws/members", <AuditNav workspaceId="ws1" />);

    expect(await screen.findByRole("link", { name: "Audit log" })).toHaveAttribute("href", "/w/ws1/settings/audit");
  });
});
