import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { TraitsPage } from "./TraitsPage";

const PLAN = {
  key: "plan_tier",
  type: "string",
  precedence: "source_priority",
  source_order: ["crm", "shop"],
  pii: false,
  channel_address: false,
  inferred: false,
  archived: false,
  description: "Subscription plan",
  created_at: "2026-10-01T09:00:00Z",
  updated_at: "2026-10-01T09:00:00Z",
};
const EMAIL = { ...PLAN, key: "contact_email", precedence: "latest", source_order: [], pii: true, channel_address: true, inferred: true, description: "" };

function members(role: "admin" | "member", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function mountAsAdmin(traits: unknown[]) {
  return mockApi({
    "GET /api/v1/session": () => json(session(OWNER, "csrf-traits")),
    "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
    "GET /api/v1/workspaces/ws1/traits": () => json(traits),
  });
}

describe("fields", () => {
  it("lists every field with its type, rule, flags and badges", async () => {
    mountAsAdmin([PLAN, EMAIL, { ...PLAN, key: "old_field", precedence: "latest", archived: true }]);
    renderRoute("/w/ws1/traits", "/w/:ws/traits", <TraitsPage />);

    const table = await screen.findByRole("table", { name: "Fields of this workspace" });
    expect(within(table).getByText("plan_tier")).toBeInTheDocument();
    expect(within(table).getByText("crm > shop")).toBeInTheDocument();
    expect(within(table).getByText("Personal data")).toBeInTheDocument();
    expect(within(table).getByText("Channel address")).toBeInTheDocument();
    expect(within(table).getByText("Inferred")).toBeInTheDocument();
    expect(within(table).getByText("Archived")).toBeInTheDocument();
  });

  it("shows an empty state, and a retry when loading fails", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/traits": () => json([]),
    });
    const first = renderRoute("/w/ws1/traits", "/w/:ws/traits", <TraitsPage />);
    expect(await screen.findByText("No fields yet")).toBeInTheDocument();
    first.unmount();

    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/traits": () => problem(500, "Internal Server Error", "internal error"),
    });
    renderRoute("/w/ws1/traits", "/w/:ws/traits", <TraitsPage />);
    expect(await screen.findByRole("button", { name: /try again|retry/i })).toBeInTheDocument();
  });

  it("is read-only for a member who is not an admin", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("member", MEMBER.id)),
      "GET /api/v1/workspaces/ws1/traits": () => json([PLAN]),
    });
    renderRoute("/w/ws1/traits", "/w/:ws/traits", <TraitsPage />);

    expect(await screen.findByText("plan_tier")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add field" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Edit/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Archive/ })).not.toBeInTheDocument();
    expect(screen.getByText(/Only workspace admins and the instance owner can add or change fields/)).toBeInTheDocument();
  });

  it("adds a field with the chosen type, rule and flags", async () => {
    let created = false;
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-traits")),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/traits": () => json(created ? [PLAN] : []),
      "POST /api/v1/workspaces/ws1/traits": () => {
        created = true;
        return json(PLAN, 201);
      },
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/traits", "/w/:ws/traits", <TraitsPage />);

    await user.click(await screen.findByRole("button", { name: "Add field" }));
    const form = within(screen.getByRole("dialog", { name: "Add a field" }));
    expect(form.getByRole("button", { name: "Add field" })).toBeDisabled();
    await user.type(form.getByLabelText("Key"), "plan_tier");
    await user.selectOptions(form.getByLabelText("When sources disagree"), "source_priority");
    await user.type(form.getByLabelText("Source order"), "crm, shop");
    await user.click(form.getByLabelText(/Personal data/));
    await user.click(form.getByRole("button", { name: "Add field" }));

    expect(await screen.findByText("Subscription plan")).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({
      key: "plan_tier",
      type: "string",
      precedence: "source_priority",
      source_order: ["crm", "shop"],
      pii: true,
      channel_address: false,
      description: "",
    });
    expect(post?.headers.get("x-csrf-token")).toBe("csrf-traits");
  });

  it("shows the server's reason when a key is taken", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/traits": () => json([PLAN]),
      "POST /api/v1/workspaces/ws1/traits": () => problem(409, "Conflict", "a trait with that key already exists"),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/traits", "/w/:ws/traits", <TraitsPage />);

    await user.click(await screen.findByRole("button", { name: "Add field" }));
    const form = within(screen.getByRole("dialog", { name: "Add a field" }));
    await user.type(form.getByLabelText("Key"), "plan_tier");
    await user.click(form.getByRole("button", { name: "Add field" }));

    expect(await form.findByText(/a trait with that key already exists/)).toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "Add a field" })).toBeInTheDocument();
  });

  it("edits only the mutable fields and sends only what changed", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-traits")),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/traits": () => json([PLAN]),
      "PATCH /api/v1/workspaces/ws1/traits/plan_tier": () => json({ ...PLAN, pii: true }),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/traits", "/w/:ws/traits", <TraitsPage />);

    await user.click(await screen.findByRole("button", { name: "Edit plan_tier" }));
    const form = within(screen.getByRole("dialog", { name: "Edit plan_tier" }));
    expect(form.queryByLabelText("Key")).not.toBeInTheDocument();
    expect(form.queryByLabelText("Type")).not.toBeInTheDocument();
    expect(form.getByRole("button", { name: "Save changes" })).toBeDisabled();
    await user.click(form.getByLabelText(/Personal data/));
    expect(form.getByRole("button", { name: "Save changes" })).toBeEnabled();
    await user.click(form.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.body).toEqual({ pii: true });
    expect(patch?.headers.get("x-csrf-token")).toBe("csrf-traits");
  });

  it("archives after confirmation and restores straight away", async () => {
    let archived = false;
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-traits")),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/traits": () => json([{ ...PLAN, archived }]),
      "PATCH /api/v1/workspaces/ws1/traits/plan_tier": async (request) => {
        archived = ((await request.clone().json()) as { archived: boolean }).archived;
        return json({ ...PLAN, archived });
      },
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/traits", "/w/:ws/traits", <TraitsPage />);

    await user.click(await screen.findByRole("button", { name: "Archive plan_tier" }));
    const confirm = within(screen.getByRole("dialog", { name: "Archive plan_tier?" }));
    await user.click(confirm.getByRole("button", { name: "Archive plan_tier" }));
    await user.click(await screen.findByRole("button", { name: "Restore plan_tier" }));

    expect(await screen.findByRole("button", { name: "Archive plan_tier" })).toBeInTheDocument();
    const patches = calls.filter((call) => call.method === "PATCH").map((call) => call.body);
    expect(patches).toEqual([{ archived: true }, { archived: false }]);
  });
});
