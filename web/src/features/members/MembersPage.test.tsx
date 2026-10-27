import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Link } from "react-router";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session, WORKSPACE } from "../../test/mockApi";
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
    expect(screen.queryByRole("switch")).not.toBeInTheDocument();
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

    const table = within(await screen.findByRole("table"));
    expect(await table.findByText("Publisher")).toBeInTheDocument();
    expect(table.getByText("Viewer")).toBeInTheDocument();
  });

  it("offers the four roles in the role select", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin")),
    });
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    const select = await screen.findByRole("combobox", { name: "Role for boss@example.test" });
    expect(within(select).getAllByRole("option").map((option) => option.textContent)).toEqual(["Admin", "Publisher", "Editor", "Viewer"]);
  });

  it("saves the Personal data switch with the member's role", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json([...members("admin"), { user_id: "u-pub", email: "pub@example.test", role: "publisher", added_at: "2026-10-03T09:00:00Z", view_pii: false }]),
      "PUT /api/v1/workspaces/ws1/members/u-pub": () => json({ user_id: "u-pub", email: "pub@example.test", role: "publisher", added_at: "2026-10-03T09:00:00Z", view_pii: true }),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    await user.click(await screen.findByRole("switch", { name: "Personal data for pub@example.test" }));

    const put = calls.find((call) => call.method === "PUT");
    expect(put?.body).toEqual({ role: "publisher", view_pii: true });
    expect(screen.getByRole("switch", { name: "Personal data for boss@example.test" })).toBeDisabled();
  });

  it("changes a role without sending view_pii", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin")),
      "PUT /api/v1/workspaces/ws1/members/u-boss": () => json({}),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    await user.selectOptions(await screen.findByRole("combobox", { name: "Role for boss@example.test" }), "viewer");

    expect(calls.find((call) => call.method === "PUT")?.body).toEqual({ role: "viewer" });
  });

  it("disables the only admin's role and remove controls and says why", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(members("editor")),
    });
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    expect(await screen.findByRole("combobox", { name: "Role for boss@example.test" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Remove boss@example.test" })).toBeDisabled();
    expect(screen.getByText("A workspace needs an admin")).toBeInTheDocument();
    expect(screen.getByRole("combobox", { name: `Role for ${MEMBER.email}` })).toBeEnabled();
  });

  it("refetches after a last_admin refusal of a role change and shows the reason when only one admin is left", async () => {
    let reads = 0;
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(members((reads += 1) === 1 ? "admin" : "editor")),
      "PUT /api/v1/workspaces/ws1/members/u-boss": () => problem(409, "Conflict", "last admin", undefined, "last_admin"),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    await user.selectOptions(await screen.findByRole("combobox", { name: "Role for boss@example.test" }), "editor");

    expect(await screen.findByText("A workspace needs an admin")).toBeInTheDocument();
    await waitFor(() => expect(calls.filter((call) => call.method === "GET" && call.path.endsWith("/members"))).toHaveLength(2));
    expect(screen.getByRole("combobox", { name: "Role for boss@example.test" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Remove boss@example.test" })).toBeDisabled();
  });

  it("leaves the controls enabled when the refetch after a last_admin refusal shows another admin", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin")),
      "PUT /api/v1/workspaces/ws1/members/u-boss": () => problem(409, "Conflict", "last admin", undefined, "last_admin"),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    await user.selectOptions(await screen.findByRole("combobox", { name: "Role for boss@example.test" }), "editor");

    await waitFor(() => expect(calls.filter((call) => call.method === "GET" && call.path.endsWith("/members"))).toHaveLength(2));
    expect(await screen.findByRole("combobox", { name: "Role for boss@example.test" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Remove boss@example.test" })).toBeEnabled();
    expect(screen.queryByText("A workspace needs an admin")).not.toBeInTheDocument();
  });

  it("shows the same reason, not a dialog error, when a removal is refused with last_admin", async () => {
    let reads = 0;
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(members((reads += 1) === 1 ? "admin" : "editor")),
      "DELETE /api/v1/workspaces/ws1/members/u-boss": () => problem(409, "Conflict", "last admin", undefined, "last_admin"),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    await user.click(await screen.findByRole("button", { name: "Remove boss@example.test" }));
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Remove boss@example.test" }));

    expect(await screen.findByText("A workspace needs an admin")).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    await waitFor(() => expect(calls.filter((call) => call.method === "GET" && call.path.endsWith("/members"))).toHaveLength(2));
    expect(screen.getByRole("button", { name: "Remove boss@example.test" })).toBeDisabled();
    expect(screen.getByRole("combobox", { name: "Role for boss@example.test" })).toBeDisabled();
  });

  it("asks before an admin demotes themselves, and cancelling sends nothing", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin")),
      "PUT /api/v1/workspaces/ws1/members/u-member": () => json({}),
      "PUT /api/v1/workspaces/ws1/members/u-boss": () => json({}),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    await user.selectOptions(await screen.findByRole("combobox", { name: `Role for ${MEMBER.email}` }), "editor");

    const dialog = screen.getByRole("dialog", { name: "Change your own role?" });
    expect(within(dialog).getByText("You will lose access to this page.")).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Keep my role" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(calls.filter((call) => call.method === "PUT")).toHaveLength(0);
    expect(screen.getByRole("combobox", { name: `Role for ${MEMBER.email}` })).toHaveValue("admin");

    await user.selectOptions(screen.getByRole("combobox", { name: "Role for boss@example.test" }), "editor");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(calls.filter((call) => call.method === "PUT")).toHaveLength(1);

    await user.selectOptions(screen.getByRole("combobox", { name: `Role for ${MEMBER.email}` }), "viewer");
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Change my role" }));
    await waitFor(() => expect(calls.filter((call) => call.method === "PUT")).toHaveLength(2));
    expect(calls.filter((call) => call.method === "PUT")[1]?.body).toEqual({ role: "viewer" });
  });

  it("does not ask the owner to confirm changing their own admin role", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () =>
        json([
          { user_id: "u-boss", email: "boss@example.test", role: "admin", added_at: "2026-10-01T09:00:00Z" },
          { user_id: OWNER.id, email: OWNER.email, role: "admin", added_at: "2026-10-02T09:00:00Z" },
        ]),
      "PUT /api/v1/workspaces/ws1/members/u-owner": () => json({}),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    await user.selectOptions(await screen.findByRole("combobox", { name: `Role for ${OWNER.email}` }), "editor");

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    await waitFor(() => expect(calls.filter((call) => call.method === "PUT")).toHaveLength(1));
  });

  it("shows the owner as Owner with the Personal data switch on and disabled and no role select or Remove", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () =>
        json([
          { user_id: OWNER.id, email: OWNER.email, role: "admin", owner: true, view_pii: false, added_at: "2026-10-01T09:00:00Z" },
          { user_id: "u-boss", email: "boss@example.test", role: "admin", owner: false, view_pii: false, added_at: "2026-10-02T09:00:00Z" },
          { user_id: "u-boss2", email: "boss2@example.test", role: "admin", owner: false, view_pii: false, added_at: "2026-10-03T09:00:00Z" },
        ]),
    });
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    const row = within((await screen.findByText(OWNER.email)).closest("tr") as HTMLElement);
    expect(row.getByText("Owner")).toBeInTheDocument();
    expect(row.queryByRole("combobox")).not.toBeInTheDocument();
    expect(row.queryByRole("button", { name: /^Remove/ })).not.toBeInTheDocument();
    const toggle = row.getByRole("switch", { name: `Personal data for ${OWNER.email}` });
    expect(toggle).toBeChecked();
    expect(toggle).toBeDisabled();
    expect(screen.getByRole("combobox", { name: "Role for boss@example.test" })).toBeEnabled();
  });

  it("lists what each role can do behind a Roles disclosure", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces": () => json([WORKSPACE]),
      "GET /api/v1/workspaces/ws1/members": () => json(members("viewer")),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/members", "/w/:ws/members", page());

    await user.click(await screen.findByText("Roles"));

    expect(screen.getByText("Access, credentials, settings")).toBeInTheDocument();
    expect(screen.getByText("Publishes flows and templates")).toBeInTheDocument();
    expect(screen.getByText("Edits drafts, runs tests")).toBeInTheDocument();
    expect(screen.getByText("Reads only")).toBeInTheDocument();
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
