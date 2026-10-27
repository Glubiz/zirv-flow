import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes } from "react-router";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { SourcePage } from "./SourcePage";
import { aiSettings, aSource, aStream, FAKE_KIND } from "./testData";

const LIST_ID = "22222222-2222-7222-8222-222222222222";

const TRAITS = [
  {
    key: "display_name",
    type: "string",
    archived: false,
    channel_address: false,
    created_at: "2026-10-01T09:00:00Z",
    description: "",
    inferred: false,
    pii: false,
    precedence: "latest",
    source_order: [],
    updated_at: "2026-10-01T09:00:00Z",
  },
];
const LISTS = [
  { id: LIST_ID, name: "Newsletter", description: "", member_count: 0, created_at: "2026-10-01T09:00:00Z", updated_at: "2026-10-01T09:00:00Z" },
];

function members(role: "admin" | "editor", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

const PREVIEW = {
  read: 2,
  truncated: true,
  records: [
    {
      key: "r1",
      deleted: false,
      result: "mapped",
      identifiers: [{ kind: "email", value: "ada@example.com" }],
      traits: { display_name: "Ada" },
      lists: [LIST_ID],
      event: { name: "order_placed", occurred_at: "2026-03-02T00:00:00Z", properties: { total: 9 } },
      errors: [],
    },
    {
      key: "r2",
      deleted: false,
      result: "rejected",
      identifiers: [],
      traits: {},
      lists: [],
      event: null,
      errors: [{ field: "profile.email", reason: "invalid_identifier" }],
    },
  ],
};

const ROUTES = {
  "GET /api/v1/workspaces/ws1/ai/settings": () => json(aiSettings(false)),
  "GET /api/v1/session": () => json(session(OWNER, "csrf-map")),
  "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
  "GET /api/v1/source-kinds": () => json([FAKE_KIND]),
  "GET /api/v1/workspaces/ws1/sources/src-1": () => json(aSource()),
  "GET /api/v1/workspaces/ws1/sources/src-1/streams": () => json([aStream()]),
  "GET /api/v1/workspaces/ws1/sources/src-1/runs": () => json({ runs: [], next_cursor: null }),
  "GET /api/v1/workspaces/ws1/traits": () => json(TRAITS),
  "GET /api/v1/workspaces/ws1/lists": () => json(LISTS),
};

const PREVIEW_PATH = "POST /api/v1/workspaces/ws1/sources/src-1/streams/stream_1/preview";
const PUT_PATH = "PUT /api/v1/workspaces/ws1/sources/src-1/streams/stream_1";

function mount() {
  return renderRoute(
    "/w/ws1/sources/src-1",
    "/w/:ws/sources/*",
    <Routes>
      <Route path=":sourceId" element={<SourcePage />} />
    </Routes>,
  );
}

async function openDialog(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole("button", { name: "Map stream_1" }));
  return screen.findByRole("dialog", { name: "Map stream_1" });
}

describe("the mapping dialog", () => {
  it("shows a loading state while the fields and lists load", async () => {
    mockApi({ ...ROUTES, "GET /api/v1/workspaces/ws1/traits": () => new Promise<Response>(() => undefined) });
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("button", { name: "Map stream_1" }));
    expect(await screen.findByText("Loading your fields and lists")).toBeInTheDocument();
  });

  it("shows an error when the lists cannot be loaded", async () => {
    mockApi({ ...ROUTES, "GET /api/v1/workspaces/ws1/lists": () => problem(500, "Internal Server Error", "internal error") });
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("button", { name: "Map stream_1" }));

    const alert = await screen.findByRole("alert");

    expect(alert).toHaveTextContent("Internal Server Error");
    expect(within(alert).getByRole("button", { name: "Try again" })).toBeInTheDocument();
  });

  it("sets a nested path, a list row and an event, previews them and saves them with the stream settings", async () => {
    const calls = mockApi({
      ...ROUTES,
      [PREVIEW_PATH]: () => json(PREVIEW),
      [PUT_PATH]: () => json(aStream({ enabled: true })),
    });
    const user = userEvent.setup();
    mount();
    const dialog = within(await openDialog(user));

    await user.type(dialog.getByLabelText("Field of row 1"), "profile.email");
    await user.click(dialog.getByRole("button", { name: "Add a row" }));
    await user.type(dialog.getByLabelText("Field of row 2"), "flags.news");
    await user.selectOptions(dialog.getByLabelText("What flags.news becomes"), `list:${LIST_ID}`);
    await user.click(dialog.getByLabelText("Each record is also an event"));
    await user.type(dialog.getByLabelText("Event name"), "order_placed");
    await user.type(dialog.getByLabelText("Time field"), "order.at");
    await user.click(dialog.getByRole("button", { name: "Add a property" }));
    await user.type(dialog.getByLabelText("Name of property 1"), "total");
    await user.type(dialog.getByLabelText("Field of property 1"), "order.total");

    expect(dialog.getByText(/Preview reads the first records/)).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Preview" }));

    const table = await dialog.findByRole("table", { name: "What the mapping makes of the first records" });
    expect(within(table).getByText("email: ada@example.com")).toBeInTheDocument();
    expect(within(table).getByText("Newsletter")).toBeInTheDocument();
    expect(within(table).getByText(/order_placed at 2026-03-02/)).toBeInTheDocument();
    expect(within(table).getByText("profile.email: the identifier is not valid")).toBeInTheDocument();
    expect(dialog.getByText("The stream has more records than the 2 shown.")).toBeInTheDocument();
    const preview = calls.find((call) => call.path.endsWith("/preview"));
    expect(preview?.body).toEqual({
      mapping: {
        key_column: null,
        columns: [
          { column: "profile.email", target: { type: "identifier", kind: "email" }, transforms: [] },
          { column: "flags.news", target: { type: "list", list_id: LIST_ID }, transforms: [] },
        ],
        event: {
          name: "order_placed",
          timestamp: { column: "order.at", transforms: [] },
          properties: [{ property: "total", column: "order.total", transforms: [] }],
        },
      },
    });

    await user.click(dialog.getByRole("button", { name: "Save mapping" }));

    await screen.findByRole("button", { name: "Map stream_1" });
    const put = calls.find((call) => call.method === "PUT");
    expect(put?.body).toMatchObject({
      enabled: false,
      sync_mode: "full_refresh",
      primary_key: ["id"],
      mapping: { event: { name: "order_placed" } },
    });
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("keeps the delete signal, the event, a list, the key and the stream's schedule and limit when it saves", async () => {
    const saved = aStream({
      enabled: true,
      sync_mode: "incremental",
      reconcile_schedule: "0 3 * * *",
      sweep_limit: 0.25,
      mapping: {
        key_column: "id",
        columns: [
          { column: "profile.email", target: { type: "identifier", kind: "email" }, transforms: [] },
          { column: "flags.news", target: { type: "list", list_id: LIST_ID }, transforms: [] },
          { column: "legacy", target: { type: "ignore" }, transforms: [] },
        ],
        event: { name: "order_placed", properties: [{ property: "sku", column: "order.sku", transforms: [] }] },
        delete_when: { column: "meta.deleted", rule: "truthy" },
      },
    });
    const calls = mockApi({
      ...ROUTES,
      "GET /api/v1/workspaces/ws1/sources/src-1/streams": () => json([saved]),
      [PUT_PATH]: () => json(saved),
    });
    const user = userEvent.setup();
    mount();
    const dialog = within(await openDialog(user));

    expect(dialog.getByLabelText("Deleted column")).toHaveValue("meta.deleted");
    expect(dialog.getByLabelText("A record is deleted when the column")).toHaveValue("truthy");
    await user.click(dialog.getByRole("button", { name: "Save mapping" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true));
    const put = calls.find((call) => call.method === "PUT")!;
    expect(put.body).toMatchObject({ reconcile_schedule: "0 3 * * *", sweep_limit: 0.25 });
    expect((put.body as { mapping: unknown }).mapping).toEqual(saved.mapping);
  });

  it("edits the delete signal and clears it again", async () => {
    const saved = aStream({
      mapping: {
        key_column: null,
        columns: [{ column: "email", target: { type: "identifier", kind: "email" }, transforms: [] }],
        delete_when: { column: "updated_at", rule: "present" },
      },
    });
    const calls = mockApi({
      ...ROUTES,
      "GET /api/v1/workspaces/ws1/sources/src-1/streams": () => json([saved]),
      [PUT_PATH]: () => json(saved),
    });
    const user = userEvent.setup();
    mount();
    const dialog = within(await openDialog(user));

    await user.selectOptions(dialog.getByLabelText("A record is deleted when the column"), "truthy");
    await user.clear(dialog.getByLabelText("Deleted column"));
    await user.type(dialog.getByLabelText("Deleted column"), "meta.gone");
    await user.click(dialog.getByRole("button", { name: "Save mapping" }));
    await waitFor(() => expect(calls.filter((call) => call.method === "PUT")).toHaveLength(1));
    expect((calls.find((call) => call.method === "PUT")!.body as { mapping: object }).mapping).toMatchObject({
      delete_when: { column: "meta.gone", rule: "truthy" },
    });

    const again = within(await openDialog(user));
    await user.clear(again.getByLabelText("Deleted column"));
    expect(again.queryByLabelText("A record is deleted when the column")).not.toBeInTheDocument();
    await user.click(again.getByRole("button", { name: "Save mapping" }));
    await waitFor(() => expect(calls.filter((call) => call.method === "PUT")).toHaveLength(2));
    const cleared = calls.filter((call) => call.method === "PUT")[1]!;
    expect((cleared.body as { mapping: object }).mapping).not.toHaveProperty("delete_when");
  });

  it("offers no preview or save until the mapping identifies a person, and says why", async () => {
    mockApi({ ...ROUTES });
    const user = userEvent.setup();
    mount();
    const dialog = within(await openDialog(user));

    expect(dialog.getByRole("button", { name: "Preview" })).toBeDisabled();
    expect(dialog.getByRole("button", { name: "Save mapping" })).toBeDisabled();
    expect(dialog.getByText("Name the field of every row.")).toBeInTheDocument();
    await user.type(dialog.getByLabelText("Field of row 1"), "name");
    expect(dialog.getByRole("button", { name: "Preview" })).toBeEnabled();
  });

  it("disables the event for a stream without a primary key and says what to do", async () => {
    mockApi({ ...ROUTES, "GET /api/v1/workspaces/ws1/sources/src-1/streams": () => json([aStream({ primary_key: [] })]) });
    const user = userEvent.setup();
    mount();
    const dialog = within(await openDialog(user));

    expect(dialog.getByLabelText("Each record is also an event")).toBeDisabled();
    expect(dialog.getByText(/Choose a primary key for this stream first/)).toBeInTheDocument();
  });

  it("points to Fields and Lists when there are none yet", async () => {
    mockApi({
      ...ROUTES,
      "GET /api/v1/workspaces/ws1/traits": () => json([]),
      "GET /api/v1/workspaces/ws1/lists": () => json([]),
    });
    const user = userEvent.setup();
    mount();
    const dialog = within(await openDialog(user));

    expect(dialog.getByText(/Your account has no fields yet/)).toBeInTheDocument();
    expect(dialog.getByText(/There are no lists yet/)).toBeInTheDocument();
  });

  it("shows an empty state when the source returns no records and an error when the preview fails", async () => {
    let answer = 0;
    mockApi({
      ...ROUTES,
      [PREVIEW_PATH]: () => {
        answer += 1;
        return answer === 1
          ? json({ read: 0, truncated: false, records: [] })
          : problem(502, "Bad Gateway", "the source could not be reached");
      },
    });
    const user = userEvent.setup();
    mount();
    const dialog = within(await openDialog(user));
    await user.type(dialog.getByLabelText("Field of row 1"), "name");

    await user.click(dialog.getByRole("button", { name: "Preview" }));
    expect(await dialog.findByText("The source returned no records")).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Preview" }));

    expect(await dialog.findByRole("alert")).toHaveTextContent("Bad Gateway");
    expect(dialog.queryByText("The source returned no records")).not.toBeInTheDocument();
  });

  it("shows the server's reason when a save is refused and keeps the dialog open", async () => {
    mockApi({
      ...ROUTES,
      [PUT_PATH]: () => problem(422, "Unprocessable Entity", 'mapping: unknown_column (column "mail")'),
    });
    const user = userEvent.setup();
    mount();
    const dialog = within(await openDialog(user));
    await user.type(dialog.getByLabelText("Field of row 1"), "mail");

    await user.click(dialog.getByRole("button", { name: "Save mapping" }));

    expect(await dialog.findByRole("alert")).toHaveTextContent('mapping: unknown_column (column "mail")');
    expect(screen.getByRole("dialog", { name: "Map stream_1" })).toBeInTheDocument();
  });

  it("opens on a saved mapping, nested paths and event included", async () => {
    const saved = aStream({
      mapping: {
        key_column: null,
        columns: [{ column: "profile.email", target: { type: "identifier", kind: "email" }, transforms: [] }],
        event: { name: "order_placed", properties: [{ property: "sku", column: "order.items.0.sku", transforms: [] }] },
      },
    });
    mockApi({ ...ROUTES, "GET /api/v1/workspaces/ws1/sources/src-1/streams": () => json([saved]) });
    const user = userEvent.setup();
    mount();
    const dialog = within(await openDialog(user));

    expect(dialog.getByLabelText("Field of row 1")).toHaveValue("profile.email");
    expect(dialog.getByLabelText("Each record is also an event")).toBeChecked();
    expect(dialog.getByLabelText("Event name")).toHaveValue("order_placed");
    expect(dialog.getByLabelText("Field of property 1")).toHaveValue("order.items.0.sku");
  });

  it("is not offered to someone who is not an admin", async () => {
    mockApi({
      ...ROUTES,
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("editor", MEMBER.id)),
    });
    mount();

    expect(await screen.findByText("You can look at this source but not change it.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Map stream_1" })).not.toBeInTheDocument();
  });

  it("loads an accepted draft into the rows and sends no save until Save is pressed", async () => {
    const content = {
      key_column: null,
      columns: [{ column: "email", target: { type: "identifier", kind: "email" }, transforms: [] }],
    };
    const calls = mockApi({
      ...ROUTES,
      "GET /api/v1/workspaces/ws1/ai/settings": () => json(aiSettings(true)),
      "POST /api/v1/workspaces/ws1/ai/drafts": () =>
        json({ id: "d1", kind: "mapping", status: "valid", attempts: 1, content, issues: [], changes: [], dry_run: null }),
      "POST /api/v1/workspaces/ws1/ai/drafts/d1/accept": () =>
        json({ id: "d1", kind: "mapping", source_id: "src-1", stream: "stream_1", content }),
    });
    const user = userEvent.setup();
    mount();
    const dialog = within(await openDialog(user));

    await user.type(await dialog.findByLabelText("What do you want?"), "Map the email");
    await user.click(dialog.getByRole("button", { name: "Draft" }));
    await user.click(await dialog.findByRole("button", { name: "Accept" }));

    expect(await dialog.findByText("Loaded into the editor. Save to apply it.")).toBeInTheDocument();
    expect(dialog.getByLabelText("Field of row 1")).toHaveValue("email");
    expect(calls.some((call) => call.method === "PUT")).toBe(false);
  });
});
