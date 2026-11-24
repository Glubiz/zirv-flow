import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { json, MEMBER, mockApi, OWNER, session, WORKSPACE } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { WorkspacesProvider } from "../workspaces";
import { MembersPage } from "./MembersPage";

const MEMBERS = [
  { user_id: "u-boss", email: "boss@example.test", role: "admin", added_at: "2026-10-01T09:00:00Z" },
  { user_id: MEMBER.id, email: MEMBER.email, role: "viewer", added_at: "2026-10-02T09:00:00Z" },
];

const PENDING = {
  id: "inv-1",
  email: "cy@acme.test",
  role: "viewer",
  view_pii: false,
  invited_by: "u-boss",
  created_at: "2026-10-05T09:00:00Z",
  expires_at: "2026-10-12T09:00:00Z",
};

function page() {
  return (
    <WorkspacesProvider>
      <MembersPage />
    </WorkspacesProvider>
  );
}

describe("invitations on the members page", () => {
  it("lists a pending invitation in the members table with its role, a Pending pill and Revoke", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(MEMBERS),
      "GET /api/v1/workspaces/ws1/invitations": () => json([PENDING]),
    });
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    const row = (await screen.findByText("cy@acme.test")).closest("tr");
    expect(row).not.toBeNull();
    expect(within(row as HTMLElement).getByText("Viewer")).toBeInTheDocument();
    expect(within(row as HTMLElement).getByText("Pending")).toBeInTheDocument();
    expect(within(row as HTMLElement).getByRole("button", { name: "Revoke invitation for cy@acme.test" })).toBeInTheDocument();
  });

  it("asks before revoking, deletes the invitation and reloads the list", async () => {
    let reads = 0;
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(MEMBERS),
      "GET /api/v1/workspaces/ws1/invitations": () => json((reads += 1) === 1 ? [PENDING] : []),
      "DELETE /api/v1/workspaces/ws1/invitations/inv-1": () => new Response(null, { status: 204 }),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    await user.click(await screen.findByRole("button", { name: "Revoke invitation for cy@acme.test" }));
    const dialog = screen.getByRole("dialog");
    expect(calls.some((call) => call.method === "DELETE")).toBe(false);
    await user.click(within(dialog).getByRole("button", { name: "Revoke" }));

    await waitFor(() => expect(screen.queryByText("cy@acme.test")).not.toBeInTheDocument());
    expect(calls.filter((call) => call.method === "DELETE")).toHaveLength(1);
    expect(calls.filter((call) => call.method === "GET" && call.path.endsWith("/invitations"))).toHaveLength(2);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("creates an invitation, shows its link once, copies it and shows the new pending row after Done", async () => {
    let created = false;
    const writeText = vi.fn(() => Promise.resolve());
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(MEMBERS),
      "GET /api/v1/workspaces/ws1/invitations": () => json(created ? [PENDING] : []),
      "POST /api/v1/workspaces/ws1/invitations": () => {
        created = true;
        return json(
          { id: "inv-1", email: "cy@acme.test", role: "editor", view_pii: true, expires_at: PENDING.expires_at, link: "https://flow.test/invite/Zx9token" },
          201,
        );
      },
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    await user.click(await screen.findByRole("button", { name: "Invite" }));
    const dialog = screen.getByRole("dialog", { name: "Invite" });
    await user.type(within(dialog).getByLabelText("Email"), "cy@acme.test");
    await user.selectOptions(within(dialog).getByLabelText("Role"), "editor");
    await user.selectOptions(within(dialog).getByLabelText("Expires"), "30");
    await user.click(within(dialog).getByRole("checkbox", { name: "Personal data" }));
    await user.click(within(dialog).getByRole("button", { name: "Create invite" }));

    expect(await screen.findByDisplayValue("https://flow.test/invite/Zx9token")).toBeInTheDocument();
    expect(calls.find((call) => call.method === "POST")?.body).toEqual({
      email: "cy@acme.test",
      role: "editor",
      view_pii: true,
      expires_in_days: 30,
    });
    await user.click(screen.getByRole("button", { name: "Copy the invitation link" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Copy the invitation link" })).toHaveTextContent("Copied"));
    await user.click(screen.getByRole("button", { name: "Done" }));

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.queryByDisplayValue("https://flow.test/invite/Zx9token")).not.toBeInTheDocument();
    expect(await screen.findByText("cy@acme.test")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Invite" }));
    expect(screen.queryByDisplayValue("https://flow.test/invite/Zx9token")).not.toBeInTheDocument();
    expect(screen.getByLabelText("Email")).toHaveValue("");
  });

  it("shows no Invite button, no pending rows and never asks for them to someone who is not an admin", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(MEMBERS),
      "GET /api/v1/workspaces/ws1/invitations": () => json([PENDING]),
    });
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    expect(await screen.findByText("boss@example.test")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Invite" })).not.toBeInTheDocument();
    expect(screen.queryByText("cy@acme.test")).not.toBeInTheDocument();
    expect(calls.some((call) => call.path.endsWith("/invitations"))).toBe(false);
  });

  it("turns the personal data box on and locks it for an admin invitation", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(MEMBERS),
      "GET /api/v1/workspaces/ws1/invitations": () => json([]),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    await user.click(await screen.findByRole("button", { name: "Invite" }));
    const box = screen.getByRole("checkbox", { name: "Personal data" });
    expect(box).not.toBeChecked();
    await user.selectOptions(screen.getByLabelText("Role"), "admin");

    expect(box).toBeChecked();
    expect(box).toBeDisabled();
  });
});
