import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createMemoryRouter, RouterProvider } from "react-router";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, problem, session } from "../../test/mockApi";
import { SessionProvider } from "../session";
import { InvitePage } from "./InvitePage";
import { PENDING_INVITE_KEY } from "./pendingInvite";

const LOOKUP = { workspace_name: "Acme", role: "viewer", email_hint: "m***@example.test" };
const SIGNED_OUT = () => problem(401, "Unauthorized", "not signed in");

function renderInvite() {
  const router = createMemoryRouter(
    [
      {
        path: "/invite/:token",
        element: (
          <SessionProvider>
            <InvitePage />
          </SessionProvider>
        ),
      },
      { path: "/sign-in", element: <p>The sign-in page</p> },
      { path: "/w/:ws", element: <p>The workspace</p> },
    ],
    { initialEntries: ["/invite/tok-1"] },
  );
  render(<RouterProvider router={router} />);
  return router;
}

beforeEach(() => globalThis.sessionStorage.clear());
afterEach(() => globalThis.sessionStorage.clear());

describe("invitation page", () => {
  it("names the workspace, the role and the hidden address, and offers to sign in", async () => {
    mockApi({
      "GET /api/v1/session": SIGNED_OUT,
      "GET /api/v1/session/methods": () => json({ local: true, oidc: false }),
      "GET /api/v1/invitations/tok-1": () => json(LOOKUP),
    });
    renderInvite();

    expect(await screen.findByRole("heading", { name: "Join Acme" })).toBeInTheDocument();
    expect(screen.getByText("Viewer · m***@example.test")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Sign in to join" })).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: "Create an account" })).toBeInTheDocument();
  });

  it("says a dead link is no longer valid, whatever made it dead", async () => {
    mockApi({
      "GET /api/v1/session": SIGNED_OUT,
      "GET /api/v1/session/methods": () => json({ local: true, oidc: false }),
      "GET /api/v1/invitations/tok-1": () => problem(404, "Not Found", "no such resource"),
    });
    renderInvite();

    expect(await screen.findByText("This invitation is no longer valid")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /join/i })).not.toBeInTheDocument();
  });

  it("keeps the token for the single sign-on redirect and goes to sign in", async () => {
    mockApi({
      "GET /api/v1/session": SIGNED_OUT,
      "GET /api/v1/session/methods": () => json({ local: true, oidc: true }),
      "GET /api/v1/invitations/tok-1": () => json(LOOKUP),
    });
    const user = userEvent.setup();
    const router = renderInvite();

    await user.click(await screen.findByRole("button", { name: "Sign in to join" }));

    expect(await screen.findByText("The sign-in page")).toBeInTheDocument();
    expect(globalThis.sessionStorage.getItem(PENDING_INVITE_KEY)).toBe("tok-1");
    expect(router.state.location.state).toEqual({ from: "/invite/tok-1" });
  });

  it("forgets a waiting token once the page opens", async () => {
    globalThis.sessionStorage.setItem(PENDING_INVITE_KEY, "tok-1");
    mockApi({
      "GET /api/v1/session": SIGNED_OUT,
      "GET /api/v1/session/methods": () => json({ local: false, oidc: true }),
      "GET /api/v1/invitations/tok-1": () => json(LOOKUP),
    });
    renderInvite();

    await screen.findByRole("heading", { name: "Join Acme" });

    expect(globalThis.sessionStorage.getItem(PENDING_INVITE_KEY)).toBeNull();
    expect(screen.queryByRole("button", { name: "Create an account" })).not.toBeInTheDocument();
  });

  it("joins as the signed-in person whose address matches and opens the workspace", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/session/methods": () => json({ local: true, oidc: false }),
      "GET /api/v1/invitations/tok-1": () => json(LOOKUP),
      "POST /api/v1/invitations/tok-1/accept": () => json({ workspace_id: "ws1", role: "viewer" }),
    });
    const user = userEvent.setup();
    renderInvite();

    await user.click(await screen.findByRole("button", { name: "Join" }));

    expect(await screen.findByText("The workspace")).toBeInTheDocument();
    expect(calls.find((call) => call.method === "POST")?.body).toEqual({});
  });

  it("shows one line and Sign out when the signed-in address is not the invited one", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session({ id: "u-x", email: "other@elsewhere.test", is_owner: false })),
      "GET /api/v1/session/methods": () => json({ local: true, oidc: false }),
      "GET /api/v1/invitations/tok-1": () => json(LOOKUP),
      "DELETE /api/v1/session": () => new Response(null, { status: 204 }),
    });
    const user = userEvent.setup();
    renderInvite();

    expect(await screen.findByRole("alert")).toHaveTextContent("This invitation is for another address");
    expect(screen.queryByRole("button", { name: "Join" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Sign out" }));

    expect(await screen.findByRole("button", { name: "Sign in to join" })).toBeInTheDocument();
  });

  it("shows the server's address refusal when the page could not tell", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/session/methods": () => json({ local: true, oidc: false }),
      "GET /api/v1/invitations/tok-1": () => json(LOOKUP),
      "POST /api/v1/invitations/tok-1/accept": () =>
        problem(403, "Forbidden", "not this account", undefined, "invitation_address_mismatch"),
    });
    const user = userEvent.setup();
    renderInvite();

    await user.click(await screen.findByRole("button", { name: "Join" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("This invitation is for another address");
    expect(screen.getByRole("button", { name: "Sign out" })).toBeInTheDocument();
  });

  it("creates the account with a password, reloads the session and opens the workspace", async () => {
    let signedIn = false;
    const calls = mockApi({
      "GET /api/v1/session": () => (signedIn ? json(session(MEMBER)) : SIGNED_OUT()),
      "GET /api/v1/session/methods": () => json({ local: true, oidc: false }),
      "GET /api/v1/invitations/tok-1": () => json(LOOKUP),
      "POST /api/v1/invitations/tok-1/accept": () => {
        signedIn = true;
        return json({ workspace_id: "ws1", role: "viewer" });
      },
    });
    const user = userEvent.setup();
    renderInvite();

    await user.click(await screen.findByRole("button", { name: "Create an account" }));
    await user.type(screen.getByLabelText("Password"), "correct horse battery");
    await user.click(screen.getByRole("button", { name: "Create account" }));

    expect(await screen.findByText("The workspace")).toBeInTheDocument();
    expect(calls.find((call) => call.method === "POST")?.body).toEqual({ password: "correct horse battery" });
    await waitFor(() => expect(calls.filter((call) => call.path === "/api/v1/session").length).toBeGreaterThan(1));
  });

  it("shows why an account cannot be created and keeps the form", async () => {
    mockApi({
      "GET /api/v1/session": SIGNED_OUT,
      "GET /api/v1/session/methods": () => json({ local: true, oidc: false }),
      "GET /api/v1/invitations/tok-1": () => json(LOOKUP),
      "POST /api/v1/invitations/tok-1/accept": () =>
        problem(409, "Conflict", "an account for this address exists; sign in to accept", undefined, "account_exists"),
    });
    const user = userEvent.setup();
    renderInvite();

    await user.click(await screen.findByRole("button", { name: "Create an account" }));
    await user.type(screen.getByLabelText("Password"), "correct horse battery");
    await user.click(screen.getByRole("button", { name: "Create account" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("an account for this address exists");
    expect(screen.getByLabelText("Password")).toBeInTheDocument();
  });
});
