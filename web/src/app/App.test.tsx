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

describe("navigation without a workspace", () => {
  it("marks exactly one item as current and renders the workspace items as plain text", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([]),
    });
    renderApp("/w");

    const nav = await screen.findByRole("navigation", { name: "Main" });
    await waitFor(() => {
      expect(nav.querySelectorAll("[aria-disabled='true']")).toHaveLength(3);
    });
    expect(nav.querySelectorAll("[aria-current='page']")).toHaveLength(1);
    expect(within(nav).getByRole("link", { name: "Workspaces" })).toHaveAttribute("aria-current", "page");
    for (const name of ["Members", "API keys", "Traces"]) {
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
