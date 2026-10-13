import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes } from "react-router";
import { describe, expect, it } from "vitest";

import { json, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { SourcePage } from "./SourcePage";
import { aSource, aStream, aWebhook, FAKE_KIND, WEBHOOK_KIND } from "./testData";

const TRAITS = [
  {
    key: "plan",
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

const PREVIEW = {
  read: 2,
  truncated: false,
  records: [
    {
      key: "1",
      deleted: false,
      result: "mapped",
      identifiers: [{ kind: "email", value: "ada@example.com" }],
      traits: { plan: "pro" },
      lists: [],
      event: { name: "order_placed", occurred_at: null, properties: { total: "12" } },
      errors: [],
    },
    {
      key: "2",
      deleted: false,
      result: "rejected",
      identifiers: [],
      traits: {},
      lists: [],
      event: null,
      errors: [{ field: "", reason: "missing_identifier" }],
    },
  ],
};

const STREAM = aStream({
  id: "str-hook",
  name: "payload",
  enabled: true,
  primary_key: [],
  discovered: { fields: [], sync_modes: ["full_refresh"], default_cursor_field: null, default_primary_key: [] },
});

const ROUTES = {
  "GET /api/v1/session": () => json(session(OWNER, "csrf-sample")),
  "GET /api/v1/workspaces/ws1/members": () =>
    json([{ user_id: OWNER.id, email: OWNER.email, role: "admin", added_at: "2026-10-01T09:00:00Z" }]),
  "GET /api/v1/source-kinds": () => json([FAKE_KIND, WEBHOOK_KIND]),
  "GET /api/v1/workspaces/ws1/sources/src-hook": () =>
    json(aSource({ id: "src-hook", kind: "webhook", name: "Orders", config: {}, credential_fields_set: [] })),
  "GET /api/v1/workspaces/ws1/sources/src-hook/streams": () => json([STREAM]),
  "GET /api/v1/workspaces/ws1/webhooks": () => json([aWebhook()]),
  "GET /api/v1/workspaces/ws1/traits": () => json(TRAITS),
  "GET /api/v1/workspaces/ws1/lists": () => json([]),
};

const PREVIEW_PATH = "POST /api/v1/workspaces/ws1/sources/src-hook/streams/payload/preview";

function mount() {
  return renderRoute(
    "/w/ws1/sources/src-hook",
    "/w/:ws/sources/*",
    <Routes>
      <Route path=":sourceId" element={<SourcePage />} />
    </Routes>,
  );
}

async function openDialog(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole("button", { name: "Map payload" }));
  return within(await screen.findByRole("dialog", { name: "Map payload" }));
}

/** Types a sample without the keyboard parsing braces as key names. */
function paste(field: HTMLElement, text: string) {
  fireEvent.change(field, { target: { value: text } });
}

describe("the mapping dialog of a webhook stream", () => {
  it("asks for a sample instead of reading a source, and does not ask for a primary key", async () => {
    mockApi(ROUTES);
    const user = userEvent.setup();
    mount();

    const dialog = await openDialog(user);

    expect(dialog.getByLabelText("Sample payload")).toBeInTheDocument();
    expect(dialog.getByText("Paste a sample payload to see the result. Nothing is saved.")).toBeInTheDocument();
    expect(dialog.getByLabelText("Each record is also an event")).toBeEnabled();
    expect(dialog.queryByText(/Choose a primary key for this stream first/)).not.toBeInTheDocument();
    expect(dialog.queryByLabelText("Deleted column")).not.toBeInTheDocument();
    expect(dialog.getByRole("button", { name: "Preview" })).toBeDisabled();
  });

  it("shows a JSON error inline and does not call the server", async () => {
    const calls = mockApi(ROUTES);
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    paste(dialog.getByLabelText("Sample payload"), "{not json");

    const alert = await dialog.findByRole("alert");
    expect(alert).toHaveTextContent("That is not valid JSON.");
    expect(alert).not.toHaveTextContent("{not json");
    expect(calls.some((call) => call.path.endsWith("/preview"))).toBe(false);
  });

  it("suggests the top-level keys of the sample and still takes a typed dotted path", async () => {
    mockApi(ROUTES);
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);

    paste(dialog.getByLabelText("Sample payload"), '{"id": "1", "email": "ada@example.com", "order": {"total": 12}}');

    const options = Array.from(document.querySelectorAll("datalist option")).map((option) => option.getAttribute("value"));
    expect(options).toEqual(["id", "email", "order"]);
    const field = dialog.getByLabelText("Field of row 1");
    await user.type(field, "order.total");
    expect(field).toHaveValue("order.total");
  });

  it("previews the pasted sample with the draft mapping and shows mapped and rejected records", async () => {
    const calls = mockApi({ ...ROUTES, [PREVIEW_PATH]: () => json(PREVIEW) });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);
    await user.type(dialog.getByLabelText("Field of row 1"), "email");
    await user.selectOptions(dialog.getByLabelText("What email becomes"), "identifier:email");

    paste(dialog.getByLabelText("Sample payload"), '[{"id": "1", "email": "ada@example.com"}, {"id": "2"}]');
    await user.click(dialog.getByRole("button", { name: "Preview" }));

    const table = await dialog.findByRole("table", { name: "What the mapping makes of the first records" });
    expect(within(table).getByText("mapped")).toBeInTheDocument();
    expect(within(table).getByText("rejected")).toBeInTheDocument();
    expect(within(table).getByText(/Record: no identifier had a value/)).toBeInTheDocument();
    expect(within(table).getByText("email: ada@example.com")).toBeInTheDocument();
    const sent = calls.find((call) => call.path.endsWith("/preview"));
    expect(sent?.body).toMatchObject({
      sample: [{ id: "1", email: "ada@example.com" }, { id: "2" }],
      mapping: { columns: [{ column: "email", target: { type: "identifier", kind: "email" } }] },
    });
  });

  it("shows the refusal of the server for a preview", async () => {
    mockApi({
      ...ROUTES,
      [PREVIEW_PATH]: () => problem(422, "Unprocessable Entity", "sample: must hold only JSON objects"),
    });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);
    await user.type(dialog.getByLabelText("Field of row 1"), "email");
    await user.selectOptions(dialog.getByLabelText("What email becomes"), "identifier:email");
    paste(dialog.getByLabelText("Sample payload"), '{"email": "a"}');

    await user.click(dialog.getByRole("button", { name: "Preview" }));

    expect(await dialog.findByText("sample: must hold only JSON objects")).toBeInTheDocument();
  });

  it("saves the mapping as it is, with no key, for a stream that has none", async () => {
    const calls = mockApi({
      ...ROUTES,
      "PUT /api/v1/workspaces/ws1/sources/src-hook/streams/payload": () => json(STREAM),
    });
    const user = userEvent.setup();
    mount();
    const dialog = await openDialog(user);
    await user.type(dialog.getByLabelText("Field of row 1"), "email");
    await user.selectOptions(dialog.getByLabelText("What email becomes"), "identifier:email");

    await user.click(dialog.getByRole("button", { name: "Save mapping" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true));
    const put = calls.find((call) => call.method === "PUT");
    expect(put?.body).toMatchObject({ primary_key: [], mapping: { columns: [{ column: "email" }] } });
    expect((put?.body as { mapping: { key_column?: string | null } }).mapping.key_column ?? null).toBeNull();
  });
});
