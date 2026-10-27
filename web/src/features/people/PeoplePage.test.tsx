import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, OWNER, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { PeoplePage } from "./PeoplePage";
import { operatorsFor, parseSearchValue } from "./traitSearch";

const ADA = "aaaaaaaa-0000-4000-8000-000000000001";
const BOB = "bbbbbbbb-0000-4000-8000-000000000002";
const SEARCH = "POST /api/v1/workspaces/ws1/people/search";
const TRAITS = "GET /api/v1/workspaces/ws1/traits";

function hit(id: string, kind: string, value: string | null) {
  return { id, display: { kind, value, masked: value === null }, updated_at: "2026-10-02T09:00:00Z" };
}

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

function mount(routes: Parameters<typeof mockApi>[0] = {}) {
  const calls = mockApi({
    "GET /api/v1/session": () => json(session(OWNER, "csrf-people")),
    [TRAITS]: () => json([trait("plan", "string"), trait("age", "number"), trait("vip", "boolean"), trait("old", "string", true)]),
    ...routes,
  });
  renderRoute("/w/ws1/people", "/w/:ws/people", <PeoplePage />);
  return calls;
}

async function searchByEmail(value: string) {
  const user = userEvent.setup();
  await user.type(await screen.findByLabelText("Value"), value);
  await user.click(screen.getByRole("button", { name: "Search" }));
  return user;
}

describe("people search", () => {
  it("asks for a search before showing anything", async () => {
    const calls = mount();

    expect(await screen.findByText("Search to find people")).toBeInTheDocument();
    expect(calls.filter((call) => call.method === "POST")).toEqual([]);
  });

  it("searches by identifier and lists the people with a link to each profile", async () => {
    const calls = mount({ [SEARCH]: () => json({ people: [hit(ADA, "email", "ada@example.test")], next_after: null }) });

    await searchByEmail("  ada@example.test ");

    const table = await screen.findByRole("table", { name: "People that match, ordered by person" });
    expect(within(table).getByText("ada@example.test")).toBeInTheDocument();
    expect(within(table).getByText("Email address")).toBeInTheDocument();
    expect(within(table).getByRole("link", { name: /Open/ })).toHaveAttribute("href", `/w/ws1/people/${ADA}`);
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({ identifier: { kind: "email", value: "ada@example.test" }, limit: 50 });
    expect(post?.headers.get("x-csrf-token")).toBe("csrf-people");
  });

  it("builds an external kind from the system name", async () => {
    const calls = mount({ [SEARCH]: () => json({ people: [], next_after: null }) });
    const user = userEvent.setup();

    await user.selectOptions(await screen.findByLabelText("Kind"), "external");
    await user.type(screen.getByLabelText("System"), "crm");
    await user.type(screen.getByLabelText("Value"), "42");
    await user.click(screen.getByRole("button", { name: "Search" }));

    await screen.findByText("No people match");
    expect(calls.find((call) => call.method === "POST")?.body).toEqual({
      identifier: { kind: "external:crm", value: "42" },
      limit: 50,
    });
  });

  it("asks for a value instead of searching for nothing", async () => {
    const calls = mount();
    const user = userEvent.setup();

    await user.click(await screen.findByRole("button", { name: "Search" }));

    expect(screen.getByRole("alert")).toHaveTextContent("Enter a value to search for.");
    expect(calls.filter((call) => call.method === "POST")).toEqual([]);
  });

  it("shows hidden identifiers as Hidden with an explanation", async () => {
    mount({ [SEARCH]: () => json({ people: [hit(BOB, "email", null)], next_after: null }) });

    await searchByEmail("bob@example.test");

    const table = await screen.findByRole("table");
    expect(within(table).getByText("Hidden")).toBeInTheDocument();
    expect(within(table).getByText(/needs the personal data permission/)).toBeInTheDocument();
    expect(within(table).queryByText("bob@example.test")).not.toBeInTheDocument();
  });

  it("says when no one matches", async () => {
    mount({ [SEARCH]: () => json({ people: [], next_after: null }) });

    await searchByEmail("nobody@example.test");

    expect(await screen.findByText("No people match")).toBeInTheDocument();
  });

  it("shows a loading state while it searches", async () => {
    mount({ [SEARCH]: () => new Promise<Response>(() => undefined) });

    await searchByEmail("ada@example.test");

    expect(await screen.findByText("Searching")).toBeInTheDocument();
  });

  it("explains a search that took too long and offers to try again", async () => {
    let attempts = 0;
    mount({
      [SEARCH]: () => {
        attempts += 1;
        return attempts === 1
          ? new Response(
              JSON.stringify({
                type: "about:blank",
                title: "Service Unavailable",
                status: 503,
                detail: "the query took too long; narrow it and try again",
                code: "search_timeout",
                trace_id: null,
                request_id: null,
              }),
              { status: 503, headers: { "content-type": "application/problem+json" } },
            )
          : json({ people: [hit(ADA, "email", "ada@example.test")], next_after: null });
      },
    });

    const user = await searchByEmail("ada@example.test");

    expect(await screen.findByText("That search took too long")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByText("ada@example.test")).toBeInTheDocument();
  });

  it("loads the next page after the last person", async () => {
    const calls = mount({
      [SEARCH]: (request) =>
        request.clone().json().then((body: { after?: string }) =>
          body.after === ADA
            ? json({ people: [hit(BOB, "phone", "+4512345678")], next_after: null })
            : json({ people: [hit(ADA, "email", "ada@example.test")], next_after: ADA }),
        ),
    });

    const user = await searchByEmail("a");
    await user.click(await screen.findByRole("button", { name: "Show more" }));

    expect(await screen.findByText("+4512345678")).toBeInTheDocument();
    expect(screen.getByText("ada@example.test")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Show more" })).not.toBeInTheDocument();
    const bodies = calls.filter((call) => call.method === "POST").map((call) => call.body);
    expect(bodies[1]).toEqual({ identifier: { kind: "email", value: "a" }, limit: 50, after: ADA });
  });
});

describe("searching by a field value", () => {
  async function openFieldSearch() {
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Field value" }));
    await screen.findByLabelText("Field");
    return user;
  }

  it("offers the registered fields except archived ones, with conditions that fit the type", async () => {
    mount();
    const user = await openFieldSearch();

    const field = screen.getByLabelText("Field");
    expect(within(field).getAllByRole("option").map((option) => option.textContent)).toEqual([
      "plan (Text)",
      "age (Number)",
      "vip (Yes or no)",
    ]);
    const condition = () => within(screen.getByLabelText("Condition")).getAllByRole("option").map((o) => o.textContent);
    expect(condition()).toEqual(["is", "starts with", "has a value"]);
    await user.selectOptions(field, "age");
    expect(condition()).toContain("is at least");
    expect(condition()).not.toContain("starts with");
    await user.selectOptions(field, "vip");
    expect(condition()).toEqual(["is", "has a value"]);
  });

  it("searches a text field by prefix", async () => {
    const calls = mount({ [SEARCH]: () => json({ people: [hit(ADA, "email", "ada@example.test")], next_after: null }) });
    const user = await openFieldSearch();

    await user.selectOptions(screen.getByLabelText("Condition"), "prefix");
    await user.type(screen.getByLabelText("Value"), "pro");
    await user.click(screen.getByRole("button", { name: "Search" }));

    expect(await screen.findByText("ada@example.test")).toBeInTheDocument();
    expect(calls.find((call) => call.method === "POST")?.body).toEqual({
      trait: { key: "plan", op: "prefix", value: "pro" },
      limit: 50,
    });
  });

  it("sends a number as a number and refuses text for it", async () => {
    const calls = mount({ [SEARCH]: () => json({ people: [], next_after: null }) });
    const user = await openFieldSearch();
    await user.selectOptions(screen.getByLabelText("Field"), "age");
    await user.selectOptions(screen.getByLabelText("Condition"), "gte");

    await user.type(screen.getByLabelText("Value"), "many");
    await user.click(screen.getByRole("button", { name: "Search" }));
    expect(screen.getByRole("alert")).toHaveTextContent("Enter a number.");
    await user.clear(screen.getByLabelText("Value"));
    await user.type(screen.getByLabelText("Value"), "18");
    await user.click(screen.getByRole("button", { name: "Search" }));

    await screen.findByText("No people match");
    const posts = calls.filter((call) => call.method === "POST");
    expect(posts).toHaveLength(1);
    expect(posts[0]?.body).toEqual({ trait: { key: "age", op: "gte", value: 18 }, limit: 50 });
  });

  it("searches for any value of a field without asking for one", async () => {
    const calls = mount({ [SEARCH]: () => json({ people: [], next_after: null }) });
    const user = await openFieldSearch();

    await user.selectOptions(screen.getByLabelText("Condition"), "exists");
    expect(screen.queryByLabelText("Value")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Search" }));

    await screen.findByText("No people match");
    expect(calls.find((call) => call.method === "POST")?.body).toEqual({ trait: { key: "plan", op: "exists" }, limit: 50 });
  });

  it("sends a yes or no choice as a boolean", async () => {
    const calls = mount({ [SEARCH]: () => json({ people: [], next_after: null }) });
    const user = await openFieldSearch();
    await user.selectOptions(screen.getByLabelText("Field"), "vip");

    await user.click(screen.getByRole("button", { name: "Search" }));
    expect(screen.getByRole("alert")).toHaveTextContent("Enter a value to search for.");
    await user.selectOptions(screen.getByLabelText("Value"), "true");
    await user.click(screen.getByRole("button", { name: "Search" }));

    await screen.findByText("No people match");
    expect(calls.find((call) => call.method === "POST")?.body).toEqual({ trait: { key: "vip", op: "eq", value: true }, limit: 50 });
  });

  it("says so when there are no fields to search by", async () => {
    mount({ [TRAITS]: () => json([]) });

    await userEvent.setup().click(await screen.findByRole("button", { name: "Field value" }));

    expect(await screen.findByText("No fields to search by yet")).toBeInTheDocument();
  });
});

describe("trait search values", () => {
  it("knows which conditions fit each type", () => {
    expect(operatorsFor("string").map((o) => o.value)).toEqual(["eq", "prefix", "exists"]);
    expect(operatorsFor("date").map((o) => o.value)).toEqual(["eq", "gt", "gte", "lt", "lte", "exists"]);
    expect(operatorsFor("string_list").map((o) => o.label)).toEqual(["contains", "has a value"]);
  });

  it("turns typed text into the value of the field's type", () => {
    expect(parseSearchValue("number", "eq", "1.5")).toEqual({ ok: true, value: 1.5 });
    expect(parseSearchValue("date", "lt", "2026-01-31")).toEqual({ ok: true, value: "2026-01-31" });
    expect(parseSearchValue("date", "lt", "31/01/2026").ok).toBe(false);
    expect(parseSearchValue("timestamp", "gt", "2026-01-31T10:00:00Z")).toEqual({ ok: true, value: "2026-01-31T10:00:00Z" });
    expect(parseSearchValue("timestamp", "gt", "later").ok).toBe(false);
    expect(parseSearchValue("string", "eq", " Pro ")).toEqual({ ok: true, value: " Pro " });
    expect(parseSearchValue("string", "eq", "  ").ok).toBe(false);
    expect(parseSearchValue("number", "exists", "")).toEqual({ ok: true, value: undefined });
  });
});
