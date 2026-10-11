import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { EventSchemaPage } from "./EventSchemaPage";

const TOTAL = { name: "total", type: "number", required: true, pii: false, description: "Order value" };
const EMAIL = { name: "email", type: "string", required: false, pii: true, description: "" };

const SCHEMA = {
  name: "order",
  mode: "warn",
  description: "",
  current_version: 1,
  has_draft: true,
  inferred: false,
  retention_days: null,
  created_at: "2026-10-01T09:00:00Z",
  updated_at: "2026-10-01T09:00:00Z",
};

const VERSION = { version: 1, properties: [TOTAL], published_at: "2026-10-01T10:00:00Z", published_by: null };

function detail(overrides: Record<string, unknown> = {}) {
  return { schema: SCHEMA, draft: [TOTAL, EMAIL], versions: [VERSION], ...overrides };
}

function members(role: "admin" | "member", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

const URI = "/api/v1/workspaces/ws1/event-schemas/order";

function mountAsAdmin(routes: Parameters<typeof mockApi>[0]) {
  return mockApi({
    "GET /api/v1/session": () => json(session(OWNER, "csrf-events")),
    "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
    ...routes,
  });
}

function open() {
  return renderRoute("/w/ws1/events/schemas/order", "/w/:ws/events/schemas/:name", <EventSchemaPage />);
}

describe("event schema", () => {
  it("shows the settings, the draft and the published versions", async () => {
    mountAsAdmin({ [`GET ${URI}`]: () => json(detail()) });
    open();

    expect(await screen.findByRole("heading", { name: "order" })).toBeInTheDocument();
    expect(await screen.findByLabelText("When an event breaks the schema")).toHaveValue("warn");
    const draft = screen.getAllByRole("group", { name: /Property/ });
    expect(draft).toHaveLength(2);
    expect(within(draft[0]!).getByLabelText("Name")).toHaveValue("total");
    expect(screen.getByText("In force")).toBeInTheDocument();
    expect(screen.getByRole("table", { name: "Properties of version 1", hidden: true })).toBeInTheDocument();
  });

  it("is read-only for a member who is not an admin", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("member", MEMBER.id)),
      [`GET ${URI}`]: () => json(detail({ schema: { ...SCHEMA, retention_days: 90 } })),
    });
    open();

    expect(await screen.findByText("90 days")).toBeInTheDocument();
    expect(screen.getByText("Warn")).toBeInTheDocument();
    expect(screen.getByRole("table", { name: "Properties of the draft", hidden: true })).toBeInTheDocument();
    for (const name of ["Save settings", "Save draft", "Publish", "Add property"]) {
      expect(screen.queryByRole("button", { name })).not.toBeInTheDocument();
    }
    expect(screen.queryByRole("textbox")).not.toBeInTheDocument();
    expect(screen.getByText(/Only workspace admins and the instance owner can change this schema/)).toBeInTheDocument();
  });

  it("changes only the mode and retention that were edited", async () => {
    const calls = mountAsAdmin({
      [`GET ${URI}`]: () => json(detail()),
      [`PATCH ${URI}`]: () => json(detail({ schema: { ...SCHEMA, mode: "block", retention_days: 120 } })),
    });
    const user = userEvent.setup();
    open();

    const save = await screen.findByRole("button", { name: "Save settings" });
    expect(save).toBeDisabled();
    await user.selectOptions(screen.getByLabelText("When an event breaks the schema"), "block");
    await user.type(screen.getByLabelText("Retention in days"), "120");
    await user.click(save);

    await waitFor(() => expect(calls.some((call) => call.method === "PATCH")).toBe(true));
    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.body).toEqual({ mode: "block", retention_days: 120 });
    expect(patch?.headers.get("x-csrf-token")).toBe("csrf-events");
  });

  it("shows the server's reason when the retention is outside the operator's bounds", async () => {
    mountAsAdmin({
      [`GET ${URI}`]: () => json(detail()),
      [`PATCH ${URI}`]: () =>
        problem(422, "Unprocessable Entity", "retention_days: must lie within the retention bounds set by the operator"),
    });
    const user = userEvent.setup();
    open();

    await user.type(await screen.findByLabelText("Retention in days"), "5");
    await user.click(screen.getByRole("button", { name: "Save settings" }));

    expect(await screen.findByText(/must lie within the retention bounds/)).toBeInTheDocument();
  });

  it("edits the draft and saves it as one list of properties", async () => {
    const calls = mountAsAdmin({
      [`GET ${URI}`]: () => json(detail()),
      [`PUT ${URI}/draft`]: () => json(detail()),
    });
    const user = userEvent.setup();
    open();

    await screen.findByLabelText("When an event breaks the schema");
    const publish = screen.getByRole("button", { name: "Publish" });
    expect(publish).toBeEnabled();
    await user.click(screen.getByRole("button", { name: "Remove property 2" }));
    await user.click(screen.getByRole("button", { name: "Add property" }));
    const added = screen.getAllByRole("group", { name: /Property/ })[1]!;
    await user.type(within(added).getByLabelText("Name"), "tier");
    await user.type(within(added).getByLabelText("Allowed values"), "free, pro");
    await user.click(within(added).getByLabelText("Personal data"));
    expect(screen.getByRole("button", { name: "Publish" })).toBeDisabled();
    expect(screen.getByText("Save the draft before you publish it.")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Save draft" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true));
    expect(calls.find((call) => call.method === "PUT")?.body).toEqual({
      properties: [
        { name: "total", type: "number", required: true, pii: false, description: "Order value", enum: null },
        { name: "tier", type: "string", required: false, pii: true, description: "", enum: ["free", "pro"] },
      ],
    });
  });

  it("asks before publishing, returns focus on Escape, and publishes on confirmation", async () => {
    let published = false;
    const calls = mountAsAdmin({
      [`GET ${URI}`]: () =>
        json(published ? detail({ draft: null, schema: { ...SCHEMA, current_version: 2, has_draft: false } }) : detail()),
      [`POST ${URI}/versions`]: () => {
        published = true;
        return json(detail({ draft: null }), 201);
      },
    });
    const user = userEvent.setup();
    open();

    const publish = await screen.findByRole("button", { name: "Publish" });
    await user.click(publish);
    const dialog = screen.getByRole("dialog", { name: "Publish this draft?" });
    expect(within(dialog).getByText(/can never be changed or removed/)).toBeInTheDocument();
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(publish).toHaveFocus();
    expect(calls.some((call) => call.method === "POST")).toBe(false);

    await user.click(publish);
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Publish version" }));

    expect(await screen.findByText(/There is no draft/)).toBeInTheDocument();
    expect(calls.filter((call) => call.method === "POST")).toHaveLength(1);
  });

  it("starts a draft from the published version when there is none", async () => {
    mountAsAdmin({
      [`GET ${URI}`]: () => json(detail({ draft: null, schema: { ...SCHEMA, has_draft: false } })),
    });
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole("button", { name: "Start a draft from version 1" }));

    const rows = screen.getAllByRole("group", { name: /Property/ });
    expect(rows).toHaveLength(1);
    expect(within(rows[0]!).getByLabelText("Name")).toHaveValue("total");
    expect(screen.getByRole("button", { name: "Publish" })).toBeDisabled();
  });

  it("says so when a schema has no published version, and explains an inferred draft", async () => {
    mountAsAdmin({
      [`GET ${URI}`]: () =>
        json(detail({ versions: [], schema: { ...SCHEMA, current_version: null, inferred: true } })),
    });
    open();

    expect(await screen.findByText(/Nothing is published/)).toBeInTheDocument();
    expect(screen.getByText(/This draft was inferred from events/)).toBeInTheDocument();
  });

  it("shows an error with a retry when the schema does not exist", async () => {
    mountAsAdmin({ [`GET ${URI}`]: () => problem(404, "Not Found", "no such resource") });
    open();

    expect(await screen.findByRole("button", { name: /try again/i })).toBeInTheDocument();
    expect(screen.getByRole("alert")).toBeInTheDocument();
  });
});
