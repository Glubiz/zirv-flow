import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { EventsPage } from "./EventsPage";

const ORDER = {
  name: "Order Completed",
  mode: "block",
  description: "A finished checkout",
  current_version: 2,
  has_draft: true,
  inferred: false,
  retention_days: null,
  created_at: "2026-10-01T09:00:00Z",
  updated_at: "2026-10-02T09:00:00Z",
};
const SEEN = { ...ORDER, name: "page_seen", mode: "warn", description: "", current_version: null, has_draft: true, inferred: true };

function members(role: "admin" | "member", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function mountAsAdmin(schemas: unknown[]) {
  return mockApi({
    "GET /api/v1/session": () => json(session(OWNER, "csrf-events")),
    "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
    "GET /api/v1/workspaces/ws1/event-schemas": () => json(schemas),
  });
}

function open() {
  return renderRoute("/w/ws1/events", "/w/:ws/events", <EventsPage />);
}

describe("event schemas", () => {
  it("lists every schema with its mode, status, version and a link to its page", async () => {
    mountAsAdmin([ORDER, SEEN]);
    open();

    const table = await screen.findByRole("table", { name: "Event schemas of this workspace" });
    const order = within(table).getByRole("link", { name: "Order Completed" });
    expect(order).toHaveAttribute("href", "/w/ws1/events/schemas/Order%20Completed");
    expect(within(table).getByText("Block")).toBeInTheDocument();
    expect(within(table).getByText("Not published")).toBeInTheDocument();
    expect(within(table).getByText("Inferred")).toBeInTheDocument();
    expect(within(table).getAllByText("Draft")).toHaveLength(2);
    expect(screen.getByRole("navigation", { name: "Events" })).toBeInTheDocument();
  });

  it("shows an empty state, and a retry when loading fails", async () => {
    mountAsAdmin([]);
    const first = open();
    expect(await screen.findByText("No event schemas yet")).toBeInTheDocument();
    first.unmount();

    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/event-schemas": () => problem(500, "Internal Server Error", "internal error"),
    });
    open();
    expect(await screen.findByRole("button", { name: /try again/i })).toBeInTheDocument();
  });

  it("is read-only for a member who is not an admin", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("member", MEMBER.id)),
      "GET /api/v1/workspaces/ws1/event-schemas": () => json([ORDER]),
    });
    open();

    expect(await screen.findByText("Order Completed")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add schema" })).not.toBeInTheDocument();
    expect(screen.getByText(/Only workspace admins and the instance owner can add or change schemas/)).toBeInTheDocument();
  });

  it("adds a schema, sending the chosen mode, and lists it afterwards", async () => {
    let created = false;
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-events")),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/event-schemas": () => json(created ? [ORDER] : []),
      "POST /api/v1/workspaces/ws1/event-schemas": () => {
        created = true;
        return json({ schema: ORDER, draft: [], versions: [] }, 201);
      },
    });
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole("button", { name: "Add schema" }));
    const form = within(screen.getByRole("dialog", { name: "Add an event schema" }));
    expect(form.getByRole("button", { name: "Add schema" })).toBeDisabled();
    await user.type(form.getByLabelText("Event name"), "Order Completed");
    await user.selectOptions(form.getByLabelText("When an event breaks the schema"), "block");
    await user.click(form.getByRole("button", { name: "Add schema" }));

    expect(await screen.findByRole("link", { name: "Order Completed" })).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({ name: "Order Completed", mode: "block", description: "", properties: [] });
    expect(post?.headers.get("x-csrf-token")).toBe("csrf-events");
  });

  it("keeps the dialog open and shows the reason when the name is taken, and Escape closes it", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/event-schemas": () => json([ORDER]),
      "POST /api/v1/workspaces/ws1/event-schemas": () =>
        problem(409, "Conflict", "an event schema with that name already exists"),
    });
    const user = userEvent.setup();
    open();

    const opener = await screen.findByRole("button", { name: "Add schema" });
    await user.click(opener);
    const form = within(screen.getByRole("dialog", { name: "Add an event schema" }));
    await user.type(form.getByLabelText("Event name"), "Order Completed");
    await user.click(form.getByRole("button", { name: "Add schema" }));

    expect(await form.findByText(/an event schema with that name already exists/)).toBeInTheDocument();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(opener).toHaveFocus();
  });
});
