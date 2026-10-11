import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes } from "react-router";
import { describe, expect, it } from "vitest";

import { json, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { ContactPage } from "./ContactPage";

const CONTACT = "/api/v1/workspaces/ws1/contacts/c-1";

function trait(key: string, type: string) {
  return {
    key,
    type,
    archived: false,
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

function contact(fields: Record<string, unknown>, masked: string[] = []) {
  return {
    key: "c-1",
    person_id: "person-1",
    fields,
    masked_fields: masked,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-02T09:00:00Z",
  };
}

function mount(routes: Parameters<typeof mockApi>[0] = {}) {
  const calls = mockApi({
    "GET /api/v1/session": () => json(session(OWNER, "csrf-contact")),
    "GET /api/v1/workspaces/ws1/members": () =>
      json([{ user_id: OWNER.id, email: OWNER.email, role: "admin", added_at: "2026-10-01T09:00:00Z" }]),
    "GET /api/v1/workspaces/ws1/contact-definition": () => json(DEFINITION),
    "GET /api/v1/workspaces/ws1/traits": () =>
      json([trait("email", "string"), trait("age", "number"), trait("newsletter", "boolean")]),
    [`GET ${CONTACT}`]: () => json(contact({ email: "ada@example.test", age: 36 })),
    "GET /api/v1/workspaces/ws1/privacy/settings": () => json({ orphan_policy: "keep" }),
    ...routes,
  });
  renderRoute(
    "/w/ws1/contacts/c-1",
    "/w/:ws/*",
    <Routes>
      <Route path="contacts/:key" element={<ContactPage />} />
      <Route path="contacts" element={<p>The contact list</p>} />
    </Routes>,
  );
  return calls;
}

describe("contact", () => {
  it("shows the fields in the order of the definition, with what is not set", async () => {
    mount();

    expect(await screen.findByRole("heading", { name: "c-1" })).toBeInTheDocument();
    const terms = (await screen.findAllByRole("term")).map((term) => term.textContent);
    expect(terms).toEqual(["email · Email address", "age", "newsletter"]);
    expect(screen.getByText("ada@example.test")).toBeInTheDocument();
    expect(screen.getByText("36")).toBeInTheDocument();
    expect(screen.getByText("Not set")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Open the profile" })).toHaveAttribute("href", "/w/ws1/people/person-1");
  });

  it("hides what the viewer may not see", async () => {
    mount({ [`GET ${CONTACT}`]: () => json(contact({ email: null, age: 36 }, ["email"])) });

    expect(await screen.findByText("Hidden")).toBeInTheDocument();
    expect(screen.queryByText("ada@example.test")).not.toBeInTheDocument();
  });

  it("says when the contact does not exist", async () => {
    mount({ [`GET ${CONTACT}`]: () => problem(404, "Not Found", "no such resource") });

    expect(await screen.findByText("No such contact")).toBeInTheDocument();
  });

  it("shows a retry when loading fails", async () => {
    mount({ [`GET ${CONTACT}`]: () => problem(500, "Internal Server Error", "internal error") });

    expect(await screen.findByRole("button", { name: "Try again" })).toBeInTheDocument();
  });

  it("edits with a patch of only what changed, and removes a cleared field", async () => {
    const calls = mount({ [`PATCH ${CONTACT}`]: () => json(contact({ email: "ada@example.test", age: 37 })) });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Edit" }));
    const dialog = await screen.findByRole("dialog", { name: "Edit c-1" });
    const age = within(dialog).getByLabelText("age");
    expect(age).toHaveValue("36");
    await user.clear(age);
    await user.type(age, "37");
    await user.click(within(dialog).getByRole("button", { name: "Save changes" }));

    await waitFor(() => {
      expect(calls.some((call) => call.method === "PATCH")).toBe(true);
    });
    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.body).toEqual({ fields: { age: 37 } });
    expect(patch?.headers.get("x-csrf-token")).toBe("csrf-contact");
    expect(await screen.findByText("Saved.")).toBeInTheDocument();
  });

  it("sends null for a field the person clears", async () => {
    const calls = mount({ [`PATCH ${CONTACT}`]: () => json(contact({ email: "ada@example.test" })) });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Edit" }));
    const dialog = await screen.findByRole("dialog", { name: "Edit c-1" });
    await user.clear(within(dialog).getByLabelText("age"));
    await user.click(within(dialog).getByRole("button", { name: "Save changes" }));

    await waitFor(() => {
      expect(calls.some((call) => call.method === "PATCH")).toBe(true);
    });
    expect(calls.find((call) => call.method === "PATCH")?.body).toEqual({ fields: { age: null } });
  });

  it("keeps a hidden value unless a new one is typed", async () => {
    const calls = mount({
      [`GET ${CONTACT}`]: () => json(contact({ email: null, age: 36 }, ["email"])),
      [`PATCH ${CONTACT}`]: () => json(contact({ email: null, age: 36 }, ["email"])),
    });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Edit" }));
    const dialog = await screen.findByRole("dialog", { name: "Edit c-1" });
    const email = within(dialog).getByLabelText("email (required)");
    expect(email).toHaveValue("");
    expect(email).toHaveAttribute("placeholder", "Hidden");
    await user.selectOptions(within(dialog).getByLabelText("newsletter"), "true");
    await user.click(within(dialog).getByRole("button", { name: "Save changes" }));

    await waitFor(() => {
      expect(calls.some((call) => call.method === "PATCH")).toBe(true);
    });
    expect(calls.find((call) => call.method === "PATCH")?.body).toEqual({ fields: { newsletter: true } });
  });

  it("shows a refused edit inline on the field", async () => {
    mount({
      [`PATCH ${CONTACT}`]: () =>
        new Response(
          JSON.stringify({
            title: "Unprocessable Entity",
            status: 422,
            detail: "invalid fields: email",
            errors: [{ index: 0, field: "email", reason: "invalid_email" }],
          }),
          { status: 422, headers: { "content-type": "application/problem+json" } },
        ),
    });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Edit" }));
    const dialog = await screen.findByRole("dialog", { name: "Edit c-1" });
    await user.clear(within(dialog).getByLabelText("email (required)"));
    await user.type(within(dialog).getByLabelText("email (required)"), "nope");
    await user.click(within(dialog).getByRole("button", { name: "Save changes" }));

    await waitFor(() => {
      expect(within(dialog).getByLabelText("email (required)")).toHaveAccessibleDescription(/Enter a valid email/);
    });
  });
});

describe("deleting a contact", () => {
  it("names what the keep policy does with the person, then deletes and goes back to the list", async () => {
    const calls = mount({ [`DELETE ${CONTACT}`]: () => new Response(null, { status: 204 }) });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete c-1?" });
    expect(await within(dialog).findByText(/keeps a person left with no record/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Delete contact" }));

    expect(await screen.findByText("The contact list")).toBeInTheDocument();
    expect(calls.some((call) => call.method === "DELETE")).toBe(true);
  });

  it("names what the delete policy does with the person", async () => {
    mount({ "GET /api/v1/workspaces/ws1/privacy/settings": () => json({ orphan_policy: "delete" }) });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Delete" }));

    const dialog = await screen.findByRole("dialog", { name: "Delete c-1?" });
    expect(await within(dialog).findByText(/they are deleted with the contact/)).toBeInTheDocument();
  });

  it("shows a failed delete and stays", async () => {
    mount({ [`DELETE ${CONTACT}`]: () => problem(503, "Service Unavailable", "try again") });
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Delete" }));
    const dialog = await screen.findByRole("dialog", { name: "Delete c-1?" });
    await user.click(within(dialog).getByRole("button", { name: "Delete contact" }));

    expect(await within(dialog).findByRole("alert")).toHaveTextContent("Service Unavailable");
  });
});
