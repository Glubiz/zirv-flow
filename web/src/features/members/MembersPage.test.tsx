import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Link } from "react-router";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, session, WORKSPACE } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { WorkspacesProvider } from "../workspaces";
import { MembersPage } from "./MembersPage";

function members(role: "admin" | "publisher" | "editor" | "viewer") {
  return [
    { user_id: "u-boss", email: "boss@example.test", role: "admin", added_at: "2026-10-01T09:00:00Z" },
    { user_id: MEMBER.id, email: MEMBER.email, role, added_at: "2026-10-02T09:00:00Z" },
  ];
}

function page() {
  return (
    <WorkspacesProvider>
      <MembersPage />
    </WorkspacesProvider>
  );
}

describe("members", () => {
  it("hides add, role and remove actions from a plain member and says why", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(members("editor")),
    });
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    expect(await screen.findByText("boss@example.test")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add member" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Remove/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("combobox")).not.toBeInTheDocument();
    expect(screen.getByText(/Only workspace admins and the instance owner can add people/)).toBeInTheDocument();
  });

  it("shows add, role and remove actions to a workspace admin", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin")),
    });
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    expect(await screen.findByRole("button", { name: "Add member" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Remove boss@example.test" })).toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: "Role for boss@example.test" })).toBeInTheDocument();
  });

  it("shows a publisher and a viewer under their own role, not as admin", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () =>
        json([
          { user_id: "u-boss", email: "boss@example.test", role: "admin", added_at: "2026-10-01T09:00:00Z" },
          { user_id: MEMBER.id, email: MEMBER.email, role: "admin", added_at: "2026-10-02T09:00:00Z" },
          { user_id: "u-pub", email: "pub@example.test", role: "publisher", added_at: "2026-10-03T09:00:00Z" },
          { user_id: "u-view", email: "view@example.test", role: "viewer", added_at: "2026-10-04T09:00:00Z" },
        ]),
    });
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    expect(await screen.findByRole("combobox", { name: "Role for pub@example.test" })).toHaveValue("publisher");
    expect(screen.getByRole("combobox", { name: "Role for view@example.test" })).toHaveValue("viewer");
  });

  it("labels each role in the read-only pill", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () =>
        json([
          { user_id: "u-pub", email: "pub@example.test", role: "publisher", added_at: "2026-10-03T09:00:00Z" },
          { user_id: MEMBER.id, email: MEMBER.email, role: "viewer", added_at: "2026-10-04T09:00:00Z" },
        ]),
    });
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    expect(await screen.findByText("Publisher")).toBeInTheDocument();
    expect(screen.getByText("Viewer")).toBeInTheDocument();
  });

  it("closes an open dialog when the user switches to another workspace", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE, { ...WORKSPACE, id: "ws2", name: "Autumn" }]),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin")),
      "GET /api/v1/workspaces/ws2/members": () => json(members("admin")),
    });
    const user = userEvent.setup();
    renderRoute(
      "/w/ws1/members",
      "/w/:ws/members",
      <>
        <Link to="/w/ws2/members">Other workspace</Link>
        {page()}
      </>,
    );

    await user.click(await screen.findByRole("button", { name: "Add member" }));
    expect(screen.getByRole("dialog", { name: "Add a member" })).toBeInTheDocument();
    await user.click(screen.getByRole("link", { name: "Other workspace" }));

    expect(await screen.findByRole("button", { name: "Add member" })).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });
});
