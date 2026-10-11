import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { ContactDefinitionPage } from "./ContactDefinitionPage";

const DEFINITION = "/api/v1/workspaces/ws1/contact-definition";

function trait(key: string, type: string, archived = false) {
  return {
    key,
    type,
    archived,
    channel_address: false,
    created_at: "2026-10-01T09:00:00Z",
    description: "",
    inferred: false,
    pii: false,
    precedence: "latest",
    source_order: [],
    updated_at: "2026-10-01T09:00:00Z",
  };
}

const TRAITS = [
  trait("email", "string"),
  trait("age", "number"),
  trait("plan", "string"),
  trait("old_field", "string", true),
];

const SAVED = {
  defined: true,
  fields: [
    { key: "email", required: true, identifier: "email" },
    { key: "age", required: false, identifier: null },
  ],
  updated_at: "2026-10-01T09:00:00Z",
  updated_by: null,
};

function mount(routes: Parameters<typeof mockApi>[0] = {}, user = OWNER) {
  const calls = mockApi({
    "GET /api/v1/session": () => json(session(user, "csrf-definition")),
    "GET /api/v1/workspaces/ws1/members": () =>
      json([
        {
          user_id: user.id,
          email: user.email,
          role: user === OWNER ? "admin" : "member",
          added_at: "2026-10-01T09:00:00Z",
        },
      ]),
    [`GET ${DEFINITION}`]: () => json(SAVED),
    "GET /api/v1/workspaces/ws1/traits": () => json(TRAITS),
    ...routes,
  });
  renderRoute("/w/ws1/contact-definition", "/w/:ws/contact-definition", <ContactDefinitionPage />);
  return calls;
}

describe("contact definition editor", () => {
  it("shows the saved fields in order with their types", async () => {
    mount();

    const list = await screen.findByRole("list", { name: "Fields in order" });
    const items = within(list).getAllByRole("listitem");
    expect(items).toHaveLength(2);
    expect(within(items[0]!).getByText("email", { selector: ".strong" })).toBeInTheDocument();
    expect(within(items[0]!).getByLabelText("Identifier for email")).toHaveValue("email");
    expect(within(items[0]!).getByRole("checkbox")).toBeChecked();
    expect(within(items[1]!).getByText("number")).toBeInTheDocument();
    expect(within(items[1]!).getByRole("checkbox")).not.toBeChecked();
  });

  it("offers only live registry fields that are not in the definition yet", async () => {
    mount();

    const add = await screen.findByLabelText("Add a field");
    const options = within(add).getAllByRole("option").map((option) => option.textContent);
    expect(options).toEqual(["Pick a field", "plan (string)"]);
  });

  it("adds a field, marks it required and an identifier, and saves the whole definition", async () => {
    const calls = mount({ [`PUT ${DEFINITION}`]: () => json({ ...SAVED, fields: [] }) });
    const user = userEvent.setup();

    await user.selectOptions(await screen.findByLabelText("Add a field"), "plan");
    await user.click(screen.getByRole("button", { name: "Add to definition" }));
    const list = screen.getByRole("list", { name: "Fields in order" });
    const plan = within(list).getAllByRole("listitem")[2]!;
    await user.click(within(plan).getByRole("checkbox"));
    await user.selectOptions(within(plan).getByLabelText("Identifier for plan"), "external");
    await user.type(within(plan).getByLabelText("System for plan"), "crm");
    await user.click(screen.getByRole("button", { name: "Save definition" }));

    await waitFor(() => {
      expect(calls.some((call) => call.method === "PUT")).toBe(true);
    });
    const put = calls.find((call) => call.method === "PUT");
    expect(put?.body).toEqual({
      fields: [
        { key: "email", required: true, identifier: "email" },
        { key: "age", required: false, identifier: null },
        { key: "plan", required: true, identifier: "external:crm" },
      ],
    });
    expect(put?.headers.get("x-csrf-token")).toBe("csrf-definition");
    expect(await screen.findByText("Saved.")).toBeInTheDocument();
  });

  it("reorders and removes fields", async () => {
    const calls = mount({ [`PUT ${DEFINITION}`]: () => json(SAVED) });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Move age up" }));
    await user.click(screen.getByRole("button", { name: "Remove email" }));
    await user.click(screen.getByRole("button", { name: "Save definition" }));

    await waitFor(() => {
      expect(calls.some((call) => call.method === "PUT")).toBe(true);
    });
    expect(calls.find((call) => call.method === "PUT")?.body).toEqual({
      fields: [{ key: "age", required: false, identifier: null }],
    });
  });

  it("shows the refusal of the server on the row it is about", async () => {
    mount({
      [`PUT ${DEFINITION}`]: () =>
        new Response(
          JSON.stringify({
            title: "Unprocessable Entity",
            status: 422,
            detail: "one or more fields cannot be used",
            errors: [{ index: 1, field: "age", reason: "identifier_needs_string" }],
          }),
          { status: 422, headers: { "content-type": "application/problem+json" } },
        ),
    });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Save definition" }));

    const alert = await screen.findByText("An identifier field must be a text field.");
    expect(alert.closest("li")).toHaveTextContent("age");
  });

  it("shows a failure that names no field as a whole", async () => {
    mount({ [`PUT ${DEFINITION}`]: () => problem(500, "Internal Server Error", "internal error") });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Save definition" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("Internal Server Error");
  });

  it("starts empty and explains it when nothing is saved", async () => {
    mount({ [`GET ${DEFINITION}`]: () => json({ defined: false, fields: [], updated_at: null, updated_by: null }) });

    expect(await screen.findByText(/No definition has been saved yet/)).toBeInTheDocument();
    expect(screen.getByText("No fields yet. A contact is only its key.")).toBeInTheDocument();
  });

  it("shows a loading state and a retry", async () => {
    let attempts = 0;
    mount({
      [`GET ${DEFINITION}`]: () => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json(SAVED);
      },
    });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("list", { name: "Fields in order" })).toBeInTheDocument();
  });

  it("is read-only for anyone who is not an admin", async () => {
    mount({}, MEMBER);

    expect(await screen.findByText("Only workspace admins can change the definition.")).toBeInTheDocument();
    const table = screen.getByRole("table", { name: "Fields of a contact, in order" });
    expect(within(table).getByText("Email address")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Save definition" })).not.toBeInTheDocument();
  });
});
