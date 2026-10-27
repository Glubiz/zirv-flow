import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { ListsPage } from "./ListsPage";

const NEWSLETTER = {
  id: "11111111-0000-4000-8000-000000000001",
  name: "Newsletter",
  description: "Monthly digest",
  member_count: 3,
  created_at: "2026-10-01T09:00:00Z",
  updated_at: "2026-10-01T09:00:00Z",
  created_by: null,
};
const VIPS = { ...NEWSLETTER, id: "11111111-0000-4000-8000-000000000002", name: "VIPs", description: "", member_count: 1 };

function members(role: "admin" | "editor", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function mount() {
  return renderRoute("/w/ws1/lists", "/w/:ws/lists", <ListsPage />);
}

describe("lists", () => {
  it("lists every list with its description and how many people are in it", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/lists": () => json([NEWSLETTER, VIPS]),
    });
    mount();

    const table = await screen.findByRole("table", { name: "Lists of this workspace" });
    const rows = within(table).getAllByRole("row");
    expect(within(rows[1]!).getByRole("link", { name: "Newsletter" })).toHaveAttribute(
      "href",
      `/w/ws1/lists/${NEWSLETTER.id}`,
    );
    expect(within(rows[1]!).getByText("Monthly digest")).toBeInTheDocument();
    expect(within(rows[1]!).getByText("3")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("VIPs")).toBeInTheDocument();
  });

  it("shows a loading state while the lists load", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/lists": () => new Promise<Response>(() => undefined),
    });
    mount();

    expect(await screen.findByText("Loading lists")).toBeInTheDocument();
  });

  it("shows an empty state that offers a first list", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/lists": () => json([]),
    });
    mount();

    expect(await screen.findByText("No lists yet")).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "New list" })).toHaveLength(2);
  });

  it("shows a retry when loading fails, and loads again", async () => {
    let attempts = 0;
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/lists": () => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json([VIPS]);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByText("VIPs")).toBeInTheDocument();
    expect(attempts).toBe(2);
  });

  it("is read-only for someone who cannot change anything", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", "someone-else")),
      "GET /api/v1/workspaces/ws1/lists": () => json([NEWSLETTER]),
    });
    mount();

    expect(await screen.findByText("Newsletter")).toBeInTheDocument();
    expect(await screen.findByText("You can look at lists but not change them.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "New list" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Edit/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Delete/ })).not.toBeInTheDocument();
  });

  it("lets a member who is not an admin manage lists", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("editor", MEMBER.id)),
      "GET /api/v1/workspaces/ws1/lists": () => json([NEWSLETTER]),
    });
    mount();

    expect(await screen.findByRole("button", { name: "Edit Newsletter" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "New list" })).toBeInTheDocument();
  });

  it("creates a list with its name and description and shows it", async () => {
    let created = false;
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-lists")),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/lists": () => json(created ? [NEWSLETTER] : [VIPS]),
      "POST /api/v1/workspaces/ws1/lists": () => {
        created = true;
        return json(NEWSLETTER, 201);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "New list" }));
    const form = within(screen.getByRole("dialog", { name: "New list" }));
    expect(form.getByRole("button", { name: "Create list" })).toBeDisabled();
    await user.type(form.getByLabelText("Name"), "  Newsletter ");
    await user.type(form.getByLabelText("Description"), "Monthly digest");
    await user.click(form.getByRole("button", { name: "Create list" }));

    expect(await screen.findByText("Created Newsletter.")).toBeInTheDocument();
    expect(await screen.findByText("Monthly digest")).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({ name: "Newsletter", description: "Monthly digest" });
    expect(post?.headers.get("x-csrf-token")).toBe("csrf-lists");
  });

  it("keeps the dialog open and says why when the name is taken", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/lists": () => json([NEWSLETTER]),
      "POST /api/v1/workspaces/ws1/lists": () => problem(409, "Conflict", "a list with that name already exists"),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "New list" }));
    const form = within(screen.getByRole("dialog", { name: "New list" }));
    await user.type(form.getByLabelText("Name"), "newsletter");
    await user.click(form.getByRole("button", { name: "Create list" }));

    expect(await form.findByText(/a list with that name already exists/)).toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "New list" })).toBeInTheDocument();
  });

  it("renames a list and sends only the fields in the form", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-lists")),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/lists": () => json([NEWSLETTER]),
      [`PATCH /api/v1/workspaces/ws1/lists/${NEWSLETTER.id}`]: () => json({ ...NEWSLETTER, name: "Digest" }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Edit Newsletter" }));
    const form = within(screen.getByRole("dialog", { name: "Edit Newsletter" }));
    expect(form.getByRole("button", { name: "Save changes" })).toBeDisabled();
    await user.clear(form.getByLabelText("Name"));
    await user.type(form.getByLabelText("Name"), "Digest");
    await user.click(form.getByRole("button", { name: "Save changes" }));

    expect(await screen.findByText("Saved Digest.")).toBeInTheDocument();
    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.body).toEqual({ name: "Digest", description: "Monthly digest" });
  });

  it("deletes only after a confirmation that names the list and its people", async () => {
    let deleted = false;
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-lists")),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/lists": () => json(deleted ? [] : [NEWSLETTER]),
      [`DELETE /api/v1/workspaces/ws1/lists/${NEWSLETTER.id}`]: () => {
        deleted = true;
        return new Response(null, { status: 204 });
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Delete Newsletter" }));
    const confirm = within(screen.getByRole("dialog", { name: "Delete Newsletter?" }));
    expect(confirm.getByText(/has 3 people/)).toBeInTheDocument();
    expect(calls.some((call) => call.method === "DELETE")).toBe(false);
    await user.click(confirm.getByRole("button", { name: "Keep list" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Delete Newsletter" }));
    await user.click(
      within(screen.getByRole("dialog", { name: "Delete Newsletter?" })).getByRole("button", {
        name: "Delete Newsletter",
      }),
    );

    expect(await screen.findByText("Deleted Newsletter.")).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(await screen.findByText("No lists yet")).toBeInTheDocument();
    expect(calls.filter((call) => call.method === "DELETE")).toHaveLength(1);
  });

  it("closes a dialog with Escape and returns focus to the button that opened it", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/lists": () => json([NEWSLETTER]),
    });
    const user = userEvent.setup();
    mount();

    const open = await screen.findByRole("button", { name: "Delete Newsletter" });
    await user.click(open);
    expect(screen.getByRole("dialog", { name: "Delete Newsletter?" })).toBeInTheDocument();
    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(open).toHaveFocus();
  });
});
