import { screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, session, WORKSPACE } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { WorkspacesProvider } from "../workspaces";
import { MembersPage } from "./MembersPage";

function members(role: "admin" | "member") {
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
      "GET /api/v1/workspaces/ws1/members": () => json(members("member")),
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
});
