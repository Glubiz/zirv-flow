import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { ContactsPage } from "./ContactsPage";

const LIST = "GET /api/v1/workspaces/ws1/contacts";
const SEARCH = "POST /api/v1/workspaces/ws1/contacts/search";
const CREATE = "POST /api/v1/workspaces/ws1/contacts";

function trait(key: string, type: string, pii = false) {
  return {
    key,
    type,
    archived: false,
    channel_address: false,
    created_at: "2026-10-01T09:00:00Z",
    description: "",
    inferred: false,
    pii,
    precedence: "latest",
    source_order: [],
    updated_at: "2026-10-01T09:00:00Z",
  };
}

const DEFINITION = {
  defined: true,
  fields: [
    { key: "email", required: true, identifier: "email" },
    { key: "age", required: false, identifier: null },
    { key: "newsletter", required: false, identifier: null },
  ],
  updated_at: "2026-10-01T09:00:00Z",
  updated_by: null,
};

function contact(key: string, fields: Record<string, unknown>, masked: string[] = []) {
  return {
    key,
    person_id: `p-${key}`,
    fields,
    masked_fields: masked,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-02T09:00:00Z",
  };
}

function members(role: "admin" | "editor", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function mount(routes: Parameters<typeof mockApi>[0] = {}, user = OWNER) {
  const calls = mockApi({
    "GET /api/v1/session": () => json(session(user, "csrf-contacts")),
    "GET /api/v1/workspaces/ws1/members": () => json(members(user === OWNER ? "admin" : "editor", user.id)),
    "GET /api/v1/workspaces/ws1/contact-definition": () => json(DEFINITION),
    "GET /api/v1/workspaces/ws1/traits": () =>
      json([trait("email", "string"), trait("age", "number"), trait("newsletter", "boolean")]),
    ...routes,
  });
  renderRoute("/w/ws1/contacts", "/w/:ws/contacts", <ContactsPage />);
  return calls;
}

describe("contacts list", () => {
  it("lists contacts by key with the first fields of the definition", async () => {
    mount({
      [LIST]: () =>
        json({
          contacts: [
            contact("c-1", { email: "ada@example.test", age: 36, newsletter: true }),
            contact("c-2", { email: "bob@example.test" }),
          ],
          next_cursor: null,
        }),
    });

    const table = await screen.findByRole("table", { name: "Contacts, ordered by key" });
    const rows = within(table).getAllByRole("row");
    expect(within(rows[0]!).getAllByRole("columnheader").map((cell) => cell.textContent)).toEqual([
      "Key",
      "email",
      "age",
      "newsletter",
      "Updated",
    ]);
    expect(within(rows[1]!).getByRole("link", { name: "c-1" })).toHaveAttribute("href", "/w/ws1/contacts/c-1");
    expect(within(rows[1]!).getByText("ada@example.test")).toBeInTheDocument();
    expect(within(rows[1]!).getByText("Yes")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("bob@example.test")).toBeInTheDocument();
  });

  it("shows hidden values as hidden, never as the value", async () => {
    mount({
      [LIST]: () => json({ contacts: [contact("c-1", { email: null, age: 36 }, ["email"])], next_cursor: null }),
    });

    const table = await screen.findByRole("table");
    expect(within(table).getByText("Hidden")).toBeInTheDocument();
    expect(within(table).getByText("36")).toBeInTheDocument();
  });

  it("shows a loading state while the contacts load", async () => {
    mount({ [LIST]: () => new Promise<Response>(() => undefined) });

    expect(await screen.findByText("Loading contacts")).toBeInTheDocument();
  });

  it("shows an empty state that offers a first contact", async () => {
    mount({ [LIST]: () => json({ contacts: [], next_cursor: null }) });

    expect(await screen.findByText("No contacts yet")).toBeInTheDocument();
    expect(await screen.findAllByRole("button", { name: "New contact" })).toHaveLength(2);
  });

  it("shows a retry when loading fails, and loads again", async () => {
    let attempts = 0;
    mount({
      [LIST]: () => {
        attempts += 1;
        return attempts === 1
          ? problem(500, "Internal Server Error", "internal error")
          : json({ contacts: [contact("c-1", {})], next_cursor: null });
      },
    });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("link", { name: "c-1" })).toBeInTheDocument();
    expect(attempts).toBe(2);
  });

  it("pages by key with Load more", async () => {
    const calls = mount({
      [LIST]: (request) => {
        const after = new URL(request.url).searchParams.get("cursor");
        return after === null
          ? json({ contacts: [contact("a", {})], next_cursor: "cursor-1" })
          : json({ contacts: [contact("b", {})], next_cursor: null });
      },
    });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Load more" }));

    expect(await screen.findByRole("link", { name: "b" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "a" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Load more" })).not.toBeInTheDocument();
    expect(calls.filter((call) => call.method === "GET" && call.path.endsWith("/contacts")).at(-1)?.search).toContain(
      "cursor=cursor-1",
    );
  });

  it("searches with a POST so the value stays out of the URL, and can go back to everyone", async () => {
    const calls = mount({
      [LIST]: () => json({ contacts: [contact("a", {}), contact("b", {})], next_cursor: null }),
      [SEARCH]: () => json({ contacts: [contact("b", {})] }),
    });
    const user = userEvent.setup();
    await screen.findByRole("link", { name: "a" });

    await user.type(screen.getByLabelText("Search"), " bob@example.test ");
    await user.click(screen.getByRole("button", { name: "Search" }));

    const table = await screen.findByRole("table", { name: "Contacts that match" });
    expect(within(table).queryByRole("link", { name: "a" })).not.toBeInTheDocument();
    expect(within(table).getByRole("link", { name: "b" })).toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({ q: "bob@example.test", limit: 50 });
    expect(post?.path).not.toContain("bob");

    await user.click(screen.getByRole("button", { name: "Show all" }));
    expect(await screen.findByRole("link", { name: "a" })).toBeInTheDocument();
  });

  it("says when a search matches nothing", async () => {
    mount({
      [LIST]: () => json({ contacts: [contact("a", {})], next_cursor: null }),
      [SEARCH]: () => json({ contacts: [] }),
    });
    const user = userEvent.setup();
    await screen.findByRole("link", { name: "a" });

    await user.type(screen.getByLabelText("Search"), "zzz");
    await user.click(screen.getByRole("button", { name: "Search" }));

    expect(await screen.findByText("No contact matches")).toBeInTheDocument();
  });

  it("is read-only for someone who is not a member", async () => {
    mount(
      {
        "GET /api/v1/workspaces/ws1/members": () => json(members("admin", "someone-else")),
        [LIST]: () => json({ contacts: [contact("a", {})], next_cursor: null }),
      },
      MEMBER,
    );

    expect(await screen.findByText("You can look at contacts but not change them.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "New contact" })).not.toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "Definition" })).not.toBeInTheDocument();
  });

  it("offers the definition view to admins only", async () => {
    mount({ [LIST]: () => json({ contacts: [], next_cursor: null }) });

    expect(await screen.findByRole("link", { name: "Definition" })).toHaveAttribute("href", "/w/ws1/contact-definition");
  });
});

describe("new contact form", () => {
  async function openForm(routes: Parameters<typeof mockApi>[0] = {}) {
    const calls = mount({ [LIST]: () => json({ contacts: [], next_cursor: null }), ...routes });
    const user = userEvent.setup();
    await user.click((await screen.findAllByRole("button", { name: "New contact" }))[0]!);
    return { calls, user, dialog: await screen.findByRole("dialog", { name: "New contact" }) };
  }

  it("is generated from the definition, with required markers and typed inputs", async () => {
    const { dialog } = await openForm();

    expect(within(dialog).getByLabelText("Key (required)")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("email (required)")).toBeInTheDocument();
    expect(within(dialog).getByLabelText("age")).toBeInTheDocument();
    const choice = within(dialog).getByLabelText("newsletter");
    expect(choice.tagName).toBe("SELECT");
    expect(within(dialog).getByText(/Identifies the contact by email address/)).toBeInTheDocument();
  });

  it("creates the contact with typed values", async () => {
    const { calls, user, dialog } = await openForm({
      [CREATE]: () => json(contact("c-9", { email: "ada@example.test", age: 36, newsletter: true }), 201),
    });

    await user.type(within(dialog).getByLabelText("Key (required)"), " c-9 ");
    await user.type(within(dialog).getByLabelText("email (required)"), "ada@example.test");
    await user.type(within(dialog).getByLabelText("age"), "36");
    await user.selectOptions(within(dialog).getByLabelText("newsletter"), "true");
    await user.click(within(dialog).getByRole("button", { name: "Create contact" }));

    await waitFor(() => {
      expect(calls.some((call) => call.method === "POST" && call.path.endsWith("/contacts"))).toBe(true);
    });
    const post = calls.find((call) => call.method === "POST" && call.path.endsWith("/contacts"));
    expect(post?.body).toEqual({ key: "c-9", fields: { email: "ada@example.test", age: 36, newsletter: true } });
    expect(post?.headers.get("x-csrf-token")).toBe("csrf-contacts");
  });

  it("shows the errors of the server inline, by field, without values", async () => {
    const { user, dialog } = await openForm({
      [CREATE]: () =>
        new Response(
          JSON.stringify({
            title: "Unprocessable Entity",
            status: 422,
            detail: "invalid fields: email, age",
            errors: [
              { index: 0, field: "email", reason: "missing_required" },
              { index: 0, field: "age", reason: "wrong_type" },
            ],
          }),
          { status: 422, headers: { "content-type": "application/problem+json" } },
        ),
    });

    await user.type(within(dialog).getByLabelText("Key (required)"), "c-9");
    await user.type(within(dialog).getByLabelText("age"), "36");
    await user.click(within(dialog).getByRole("button", { name: "Create contact" }));

    await waitFor(() => {
      expect(within(dialog).getByLabelText("email (required)")).toBeInvalid();
    });
    expect(within(dialog).getByLabelText("email (required)")).toHaveAccessibleDescription(/This field is required/);
    expect(within(dialog).getByLabelText("age")).toHaveAccessibleDescription(/not the right type/);
    expect(within(dialog).getByLabelText("Key (required)")).not.toBeInvalid();
  });

  it("puts a duplicate key on the key field", async () => {
    const { user, dialog } = await openForm({
      [CREATE]: () => problem(409, "Conflict", "a contact with that key already exists"),
    });

    await user.type(within(dialog).getByLabelText("Key (required)"), "c-9");
    await user.type(within(dialog).getByLabelText("email (required)"), "ada@example.test");
    await user.click(within(dialog).getByRole("button", { name: "Create contact" }));

    await waitFor(() => {
      expect(within(dialog).getByLabelText("Key (required)")).toHaveAccessibleDescription(/already exists/);
    });
  });

  it("refuses text where a number is expected before asking the server", async () => {
    const { calls, user, dialog } = await openForm();

    await user.type(within(dialog).getByLabelText("Key (required)"), "c-9");
    await user.type(within(dialog).getByLabelText("age"), "old");
    await user.click(within(dialog).getByRole("button", { name: "Create contact" }));

    await waitFor(() => {
      expect(within(dialog).getByLabelText("age")).toHaveAccessibleDescription(/Enter a number/);
    });
    expect(calls.filter((call) => call.method === "POST")).toEqual([]);
  });

  it("explains that a workspace without a definition has key-only contacts", async () => {
    const { dialog } = await openForm({
      "GET /api/v1/workspaces/ws1/contact-definition": () =>
        json({ defined: false, fields: [], updated_at: null, updated_by: null }),
    });

    expect(within(dialog).getByText(/no contact definition yet/)).toBeInTheDocument();
  });
});
