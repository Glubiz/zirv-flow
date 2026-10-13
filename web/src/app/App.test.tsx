import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session, WORKSPACE } from "../test/mockApi";
import { AppRoutes } from "./App";

function renderApp(path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <AppRoutes />
    </MemoryRouter>,
  );
}

describe("first run", () => {
  it("shows the setup form when setup is required and lands signed in on the workspaces screen after submit", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => problem(401, "Unauthorized", "sign in first"),
      "GET /api/v1/setup": () => json({ required: true }),
      "POST /api/v1/setup": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
    });
    const user = userEvent.setup();
    renderApp("/");

    expect(await screen.findByRole("heading", { name: "Set up zirv flow" })).toBeInTheDocument();
    await user.type(screen.getByLabelText("Setup token"), "tok-123");
    await user.type(screen.getByLabelText("Owner email"), "owner@example.test");
    await user.type(screen.getByLabelText("Password"), "correct horse battery");
    await user.click(screen.getByRole("button", { name: "Create owner and sign in" }));

    expect(await screen.findByRole("heading", { name: "Workspaces" })).toBeInTheDocument();
    expect(await screen.findByText("Spring campaigns", { selector: "td" })).toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({ token: "tok-123", email: "owner@example.test", password: "correct horse battery" });
  });
});

describe("sign in", () => {
  it("shows the detail and trace id the API sent when the password is wrong", async () => {
    mockApi({
      "GET /api/v1/session": () => problem(401, "Unauthorized", "not signed in"),
      "GET /api/v1/setup": () => json({ required: false }),
      "GET /api/v1/version": () => json({ version: "0.10.0" }),
      "POST /api/v1/session": () => problem(401, "Unauthorized", "Invalid email or password", "trace-abc-123"),
    });
    const user = userEvent.setup();
    renderApp("/jobs");

    await user.type(await screen.findByLabelText("Email"), "owner@example.test");
    await user.type(screen.getByLabelText("Password"), "wrong");
    await user.click(screen.getByRole("button", { name: "Sign in" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Invalid email or password");
    expect(alert).toHaveTextContent("trace-abc-123");
    expect(screen.getByRole("heading", { name: "Welcome back" })).toBeInTheDocument();
  });
});

describe("sign-in methods", () => {
  it("offers only single sign-on when the instance turns local sign-in off", async () => {
    mockApi({
      "GET /api/v1/session": () => problem(401, "Unauthorized", "not signed in"),
      "GET /api/v1/setup": () => json({ required: false }),
      "GET /api/v1/version": () => json({ version: "0.10.0" }),
      "GET /api/v1/session/methods": () => json({ local: false, oidc: true }),
    });
    renderApp("/sign-in");

    const sso = await screen.findByRole("link", { name: "Continue with single sign-on" });
    expect(sso).toHaveAttribute("href", "/api/v1/session/oidc/start");
    expect(screen.queryByLabelText("Password")).not.toBeInTheDocument();
  });
});

describe("navigation", () => {
  it("hides the Jobs entry from a signed-in user who is not the instance owner", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
    });
    renderApp("/w");

    const nav = await screen.findByRole("navigation", { name: "Main" });
    await waitFor(() => {
      expect(nav).toHaveTextContent("Health");
    });
    expect(nav).not.toHaveTextContent("Jobs");
  });
});

describe("people routes", () => {
  const routes = {
    "GET /api/v1/session": () => json(session(OWNER)),
    "GET /api/v1/workspaces": () => json([WORKSPACE]),
    "GET /api/v1/workspaces/ws1/members": () => json([]),
    "GET /api/v1/workspaces/ws1/identity-conflicts": () => json({ conflicts: [], next_before: null }),
  };

  it("serves the search page as People, current in the menu", async () => {
    mockApi(routes);
    renderApp("/w/ws1/people");

    expect(await screen.findByRole("heading", { name: "People" })).toBeInTheDocument();
    expect(await screen.findByText("Search to find people")).toBeInTheDocument();
    const nav = screen.getByRole("navigation", { name: "Main" });
    expect(within(nav).getByRole("link", { name: "People" })).toHaveAttribute("aria-current", "page");
    expect(within(nav).getByRole("link", { name: "Conflicts" })).not.toHaveAttribute("aria-current");
  });

  it("keeps Conflicts current, and People not, on the conflicts page", async () => {
    mockApi(routes);
    renderApp("/w/ws1/people/conflicts");

    const nav = await screen.findByRole("navigation", { name: "Main" });
    await waitFor(() => {
      expect(within(nav).getByRole("link", { name: "Conflicts" })).toHaveAttribute("aria-current", "page");
    });
    expect(within(nav).getByRole("link", { name: "People" })).not.toHaveAttribute("aria-current");
  });

  it("reaches a profile at a person's address", async () => {
    mockApi({
      ...routes,
      "GET /api/v1/workspaces/ws1/people/p1": () => problem(404, "Not Found", "no such resource"),
    });
    renderApp("/w/ws1/people/p1");

    expect(await screen.findByRole("alert")).toHaveTextContent("no such resource");
    expect(screen.getByRole("link", { name: "All people" })).toHaveAttribute("href", "/w/ws1/people");
  });
});

describe("event routes", () => {
  it("serves the schema list, the rejects and the settings, with Events current in the menu", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json([]),
      "GET /api/v1/workspaces/ws1/event-schemas": () => json([]),
      "GET /api/v1/workspaces/ws1/ingest-rejects": () => json({ rejects: [], next_before: null }),
      "GET /api/v1/workspaces/ws1/event-policy": () =>
        json({
          unplanned: "draft",
          future_skew_seconds: 300,
          lateness_seconds: 86400,
          redaction: { drop: [], hash: [], truncate_ip: [] },
        }),
    });
    const user = userEvent.setup();
    renderApp("/w/ws1/events");

    expect(await screen.findByRole("heading", { name: "Events" })).toBeInTheDocument();
    expect(await screen.findByText("No event schemas yet")).toBeInTheDocument();
    const main = screen.getByRole("navigation", { name: "Main" });
    expect(within(main).getByRole("link", { name: "Events" })).toHaveAttribute("aria-current", "page");

    await user.click(screen.getByRole("link", { name: "Rejects" }));
    expect(await screen.findByRole("heading", { name: "Rejects" })).toBeInTheDocument();
    expect(await screen.findByText("No rejects")).toBeInTheDocument();
    await user.click(screen.getByRole("link", { name: "Settings" }));
    expect(await screen.findByRole("heading", { name: "Event settings" })).toBeInTheDocument();
    expect(within(main).getByRole("link", { name: "Events" })).toHaveAttribute("aria-current", "page");
  });
});

describe("event schema routes", () => {
  it("reaches a schema named like a static page", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json([]),
      "GET /api/v1/workspaces/ws1/event-schemas/rejects": () =>
        json({
          schema: {
            name: "rejects",
            mode: "warn",
            description: "",
            current_version: null,
            has_draft: false,
            inferred: false,
            retention_days: null,
            created_at: "2026-10-01T09:00:00Z",
            updated_at: "2026-10-01T09:00:00Z",
          },
          draft: null,
          versions: [],
        }),
    });
    renderApp("/w/ws1/events/schemas/rejects");

    expect(await screen.findByRole("heading", { name: "rejects" })).toBeInTheDocument();
    expect(await screen.findByText(/Nothing is published/)).toBeInTheDocument();
  });
});

describe("navigation without a workspace", () => {
  it("marks exactly one item as current and renders the workspace items as plain text", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([]),
    });
    renderApp("/w");

    const nav = await screen.findByRole("navigation", { name: "Main" });
    await waitFor(() => {
      expect(nav.querySelectorAll("[aria-disabled='true']")).toHaveLength(13);
    });
    expect(nav.querySelectorAll("[aria-current='page']")).toHaveLength(1);
    expect(within(nav).getByRole("link", { name: "Workspaces" })).toHaveAttribute("aria-current", "page");
    for (const name of ["Members", "Fields", "Lists", "Contacts", "People", "Conflicts", "Imports", "Events", "Topics", "Suppressions", "AI", "API keys", "Traces"]) {
      expect(within(nav).queryByRole("link", { name: new RegExp(name) })).not.toBeInTheDocument();
    }
    expect(nav).toHaveTextContent("pick a workspace first");
  });
});

describe("expired session", () => {
  it("sends the user to sign-in when a later call answers 401", async () => {
    let first = true;
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/admin/jobs": () => {
        if (first) {
          first = false;
          return json({ kinds: [] });
        }
        return problem(401, "Unauthorized", "session expired");
      },
      "GET /api/v1/admin/jobs/dead": () => json({ jobs: [], next_before: null }),
      "GET /api/v1/setup": () => json({ required: false }),
      "GET /api/v1/version": () => json({ version: "0.10.0" }),
    });
    const user = userEvent.setup();
    renderApp("/jobs");

    await user.click(await screen.findByRole("button", { name: "Refresh" }));

    expect(await screen.findByRole("heading", { name: "Welcome back" })).toBeInTheDocument();
  });
});
