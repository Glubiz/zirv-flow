import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { ConsentPanel } from "./ConsentPanel";
import { DimensionsPage } from "./DimensionsPage";

const WS = "/api/v1/workspaces/ws1";
const PERSON = "33333333-0000-4000-8000-000000000001";

const TOPIC = {
  key: "topic",
  name: "Topic",
  builtin: true,
  archived: false,
  values: [{ key: "offers", name: "Offers", archived: false }],
};
const VERTICAL = {
  key: "vertical",
  name: "Vertical",
  builtin: false,
  archived: false,
  values: [
    { key: "retail", name: "Retail", archived: false },
    { key: "sports", name: "Sports", archived: true },
  ],
};

function access(role: "admin" | "member"): Record<string, Handler> {
  const who = role === "admin" ? OWNER : MEMBER;
  return {
    "GET /api/v1/session": () => json(session(who)),
    "GET /api/v1/workspaces/ws1/members": () => json([{ user_id: who.id, email: who.email, role, added_at: "2026-10-01T09:00:00Z" }]),
  };
}

function mount() {
  return renderRoute("/w/ws1/consent/dimensions", "/w/:ws/consent/dimensions", <DimensionsPage />);
}

describe("consent dimensions", () => {
  it("lists the built-in topic first, then the declared dimensions with their values", async () => {
    mockApi({ ...access("admin"), [`GET ${WS}/consent/dimensions`]: () => json([TOPIC, VERTICAL]) });
    mount();

    const cards = await screen.findAllByRole("heading", { level: 2 });
    expect(cards.map((card) => card.textContent)).toEqual(["Topic", "Vertical"]);
    expect(screen.getByText("Built in")).toBeInTheDocument();
    const values = screen.getByRole("list", { name: "Values of Vertical" });
    expect(within(values).getByText("Retail")).toBeInTheDocument();
    expect(within(values).getByText("Archived")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Rename Topic" })).not.toBeInTheDocument();
  });

  it("declares a dimension and adds a value", async () => {
    let dimensions = [TOPIC];
    const calls = mockApi({
      ...access("admin"),
      [`GET ${WS}/consent/dimensions`]: () => json(dimensions),
      [`POST ${WS}/consent/dimensions`]: () => {
        dimensions = [TOPIC, { ...VERTICAL, values: [] }];
        return json(dimensions[1], 201);
      },
      [`POST ${WS}/consent/dimensions/vertical/values`]: () => json({ key: "retail", name: "Retail", archived: false }, 201),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Add dimension" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a dimension" });
    await user.type(within(dialog).getByLabelText("Key"), "vertical");
    await user.type(within(dialog).getByLabelText("Name"), "Vertical");
    await user.click(within(dialog).getByRole("button", { name: "Add dimension" }));

    await user.click(await screen.findByRole("button", { name: "Add a value to Vertical" }));
    const valueDialog = await screen.findByRole("dialog", { name: "Add a value to Vertical" });
    await user.type(within(valueDialog).getByLabelText("Value key"), "retail");
    await user.type(within(valueDialog).getByLabelText("Value name"), "Retail");
    await user.click(within(valueDialog).getByRole("button", { name: "Add value" }));

    await waitFor(() => expect(calls.some((call) => call.path === `${WS}/consent/dimensions/vertical/values`)).toBe(true));
    expect(calls.find((call) => call.method === "POST" && call.path === `${WS}/consent/dimensions`)?.body).toEqual({ key: "vertical", name: "Vertical" });
    expect(calls.find((call) => call.path === `${WS}/consent/dimensions/vertical/values`)?.body).toEqual({ key: "retail", name: "Retail" });
  });

  it("archives a value", async () => {
    const calls = mockApi({
      ...access("admin"),
      [`GET ${WS}/consent/dimensions`]: () => json([TOPIC, VERTICAL]),
      [`PATCH ${WS}/consent/dimensions/vertical/values/retail`]: () => json({ key: "retail", name: "Retail", archived: true }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Archive Retail" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PATCH")).toBe(true));
    expect(calls.find((call) => call.method === "PATCH")?.body).toEqual({ archived: true });
  });

  it("is read-only for a member who is not an admin", async () => {
    mockApi({ ...access("member"), [`GET ${WS}/consent/dimensions`]: () => json([TOPIC, VERTICAL]) });
    mount();

    expect(await screen.findByText("Vertical")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add dimension" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Import consent" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Rename/ })).not.toBeInTheDocument();
    expect(screen.getByText(/Only workspace admins and the instance owner/)).toBeInTheDocument();
  });

  it("shows an error with a retry", async () => {
    let fail = true;
    mockApi({
      ...access("admin"),
      [`GET ${WS}/consent/dimensions`]: () => (fail ? problem(500, "Internal Server Error", "internal error") : json([TOPIC])),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /try again/i }));
    fail = false;
    await user.click(await screen.findByRole("button", { name: /try again/i }));
    expect(await screen.findByText("Topic")).toBeInTheDocument();
  });

  it("names a dimension column in the consent import and previews before importing", async () => {
    const calls = mockApi({
      ...access("admin"),
      [`GET ${WS}/consent/dimensions`]: () => json([TOPIC, VERTICAL]),
      [`POST ${WS}/consent/import`]: (request) => {
        const dry = new URL(request.url).searchParams.get("dry_run") === "true";
        return json({ changed: 1, unchanged: 0, dry_run: dry, invalid: [{ row: 3, reason: "unknown_dimension_value" }] });
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Import consent" }));
    const dialog = await screen.findByRole("dialog", { name: "Import consent" });
    expect(within(dialog).getAllByText("person_id,channel,topic,status,dimension:vertical").length).toBeGreaterThan(0);
    expect(within(dialog).getByRole("button", { name: "Import" })).toBeDisabled();

    await user.click(within(dialog).getByLabelText("Or paste the CSV"));
    await user.paste(`person_id,channel,topic,status,dimension:vertical\n${PERSON},email,,unsubscribed,retail\n`);
    await user.click(within(dialog).getByRole("button", { name: "Preview" }));
    expect(await within(dialog).findByText(/1 state would change/)).toBeInTheDocument();
    expect(within(dialog).getByText(/Line 3: A dimension value does not exist/)).toBeInTheDocument();

    await user.click(within(dialog).getByRole("button", { name: "Import" }));
    expect(await screen.findByText(/Imported: 1 state changed/)).toBeInTheDocument();
    const posts = calls.filter((call) => call.method === "POST" && call.path === `${WS}/consent/import`);
    expect(posts.map((call) => call.search)).toEqual(["?dry_run=true", "?dry_run=false"]);
  });
});

describe("the consent panel with dimensions", () => {
  const STATES = [
    { channel: "email", status: "unsubscribed", explicit: true, changed_at: "2026-10-20T09:00:00Z", topic_key: null, topic_name: null, dimensions: { vertical: "retail" } },
    { channel: "email", status: "subscribed", explicit: true, changed_at: "2026-10-19T09:00:00Z", topic_key: null, topic_name: null, dimensions: {} },
  ];

  it("shows the dimension values a choice is scoped to and records a scoped choice", async () => {
    const calls = mockApi({
      ...access("admin"),
      [`GET ${WS}/people/${PERSON}/consent`]: () => json({ person_id: PERSON, states: STATES }),
      [`GET ${WS}/topics`]: () => json([]),
      [`GET ${WS}/consent/dimensions`]: () => json([TOPIC, VERTICAL]),
      [`PUT ${WS}/people/${PERSON}/consent`]: () => json({ person_id: PERSON, states: STATES }),
    });
    const user = userEvent.setup();
    renderRoute(`/w/ws1/people/${PERSON}`, "/w/:ws/people/:id", <ConsentPanel workspaceId="ws1" personId={PERSON} canWrite />);

    expect(await screen.findByText("vertical: retail")).toBeInTheDocument();
    const scope = await screen.findByLabelText("Vertical");
    expect(within(scope).queryByRole("option", { name: "Sports" })).not.toBeInTheDocument();
    await user.selectOptions(scope, "retail");
    await user.click(screen.getByRole("button", { name: "Record choice" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true));
    expect(calls.find((call) => call.method === "PUT")?.body).toEqual({
      channel: "email",
      status: "unsubscribed",
      topic_key: null,
      dimensions: { vertical: "retail" },
    });
  });

  it("leaves dimensions out of a choice that does not name one", async () => {
    const calls = mockApi({
      ...access("admin"),
      [`GET ${WS}/people/${PERSON}/consent`]: () => json({ person_id: PERSON, states: [] }),
      [`GET ${WS}/topics`]: () => json([]),
      [`GET ${WS}/consent/dimensions`]: () => json([TOPIC, VERTICAL]),
      [`PUT ${WS}/people/${PERSON}/consent`]: () => json({ person_id: PERSON, states: [] }),
    });
    const user = userEvent.setup();
    renderRoute(`/w/ws1/people/${PERSON}`, "/w/:ws/people/:id", <ConsentPanel workspaceId="ws1" personId={PERSON} canWrite />);

    await user.click(await screen.findByRole("button", { name: "Record choice" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true));
    expect(calls.find((call) => call.method === "PUT")?.body).not.toHaveProperty("dimensions");
  });
});
