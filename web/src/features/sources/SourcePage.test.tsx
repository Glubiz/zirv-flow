import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes, useLocation } from "react-router";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { SourcePage } from "./SourcePage";
import { aSource, aStream, FAKE_KIND } from "./testData";

function members(role: "admin" | "member", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function Landing() {
  const location = useLocation();
  return <p>Now at {location.pathname}</p>;
}

function mount() {
  return renderRoute(
    "/w/ws1/sources/src-1",
    "/w/:ws/sources/*",
    <Routes>
      <Route path=":sourceId" element={<SourcePage />} />
      <Route index element={<Landing />} />
    </Routes>,
  );
}

const ADMIN = {
  "GET /api/v1/session": () => json(session(OWNER, "csrf-source")),
  "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
  "GET /api/v1/source-kinds": () => json([FAKE_KIND]),
  "GET /api/v1/workspaces/ws1/sources/src-1": () => json(aSource()),
  "GET /api/v1/workspaces/ws1/sources/src-1/streams": () => json([aStream()]),
  "GET /api/v1/workspaces/ws1/sources/src-1/runs": () => json({ runs: [], next_cursor: null }),
};

describe("a source", () => {
  it("shows its settings, that the credential is set and never its value, and its streams", async () => {
    mockApi(ADMIN);
    mount();

    expect(await screen.findByRole("heading", { name: "CRM" })).toBeInTheDocument();
    const settings = within(await screen.findByRole("region", { name: "Settings" }));
    expect(settings.getByText("Streams")).toBeInTheDocument();
    expect(settings.getByText("Token")).toBeInTheDocument();
    expect(settings.getByText("Set")).toBeInTheDocument();
    const table = await screen.findByRole("table", { name: "Streams of this source" });
    expect(within(table).getByText("stream_1")).toBeInTheDocument();
  });

  it("shows a loading state while it loads", async () => {
    mockApi({ ...ADMIN, "GET /api/v1/workspaces/ws1/sources/src-1": () => new Promise<Response>(() => undefined) });
    mount();

    expect(await screen.findByText("Loading source")).toBeInTheDocument();
  });

  it("shows an error with a retry when it cannot be loaded", async () => {
    let attempts = 0;
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/sources/src-1": () => {
        attempts += 1;
        return attempts === 1 ? problem(404, "Not Found", "no such resource") : json(aSource());
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("heading", { name: "CRM" })).toBeInTheDocument();
  });

  it("shows an empty state for a source without streams and a loading state for them", async () => {
    mockApi({ ...ADMIN, "GET /api/v1/workspaces/ws1/sources/src-1/streams": () => json([]) });
    mount();

    expect(await screen.findByText("No streams yet")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Discover streams" })).toBeInTheDocument();
  });

  it("shows an error when the streams cannot be loaded", async () => {
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/sources/src-1/streams": () => problem(500, "Internal Server Error", "internal error"),
    });
    mount();

    expect(await screen.findByRole("heading", { name: "CRM" })).toBeInTheDocument();
    expect(await screen.findByRole("alert")).toHaveTextContent("Internal Server Error");
  });

  it("is read-only for someone who is not an admin", async () => {
    mockApi({
      ...ADMIN,
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("member", MEMBER.id)),
    });
    mount();

    expect(await screen.findByText("You can look at this source but not change it.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Edit" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Delete" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Test connection" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Set up stream_1" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Discover streams/ })).not.toBeInTheDocument();
  });

  it("reports a passing and a failing connection test", async () => {
    let tests = 0;
    mockApi({
      ...ADMIN,
      "POST /api/v1/workspaces/ws1/sources/src-1/test": () => {
        tests += 1;
        return tests === 1 ? json({ ok: true, error: null }) : json({ ok: false, error: "the source refused the credential" });
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Test connection" }));
    expect(await screen.findByText("The connection works.")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Test connection" }));

    expect(await screen.findByText("The connection test failed")).toBeInTheDocument();
    expect(screen.getByText("the source refused the credential")).toBeInTheDocument();
  });

  it("discovers streams again and reloads the table", async () => {
    let found = false;
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/sources/src-1/streams": () =>
        json(found ? [aStream(), aStream({ id: "str-2", name: "stream_2" })] : [aStream()]),
      "POST /api/v1/workspaces/ws1/sources/src-1/discover": () => {
        found = true;
        return json([aStream(), aStream({ id: "str-2", name: "stream_2" })]);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Discover streams again" }));

    expect(await screen.findByText("Found 2 streams.")).toBeInTheDocument();
    expect(await screen.findByText("stream_2")).toBeInTheDocument();
  });

  it("never prefills the credential, and replaces it only when asked to", async () => {
    const calls = mockApi({
      ...ADMIN,
      "PATCH /api/v1/workspaces/ws1/sources/src-1": () => json(aSource({ name: "CRM 2" })),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Edit" }));
    const dialog = within(screen.getByRole("dialog", { name: "Edit CRM" }));
    expect(dialog.queryByLabelText("Token")).not.toBeInTheDocument();
    expect(dialog.getByLabelText("Streams (optional)")).toHaveValue(2);
    await user.click(dialog.getByLabelText("Replace the credential"));
    const token = dialog.getByLabelText("Token");
    expect(token).toHaveAttribute("type", "password");
    expect(token).toHaveValue("");
    expect(dialog.getByRole("button", { name: "Save changes" })).toBeDisabled();
    await user.type(token, "new-token");
    await user.clear(dialog.getByLabelText("Name"));
    await user.type(dialog.getByLabelText("Name"), "CRM 2");
    await user.click(dialog.getByRole("button", { name: "Save changes" }));

    expect(await screen.findByText("Saved CRM.")).toBeInTheDocument();
    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.body).toEqual({
      name: "CRM 2",
      config: { streams: 2 },
      enabled: true,
      schedule: "",
      credential: { token: "new-token" },
    });
  });

  it("leaves the credential out of the change when it is not being replaced", async () => {
    const calls = mockApi({
      ...ADMIN,
      "PATCH /api/v1/workspaces/ws1/sources/src-1": () => json(aSource()),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Edit" }));
    await user.click(within(screen.getByRole("dialog", { name: "Edit CRM" })).getByRole("button", { name: "Save changes" }));

    await screen.findByText("Saved CRM.");
    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.body).not.toHaveProperty("credential");
  });

  it("deletes only after a confirmation, and returns to the list", async () => {
    const calls = mockApi({
      ...ADMIN,
      "DELETE /api/v1/workspaces/ws1/sources/src-1": () => new Response(null, { status: 204 }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Delete" }));
    const dialog = within(screen.getByRole("dialog", { name: "Delete CRM?" }));
    expect(calls.some((call) => call.method === "DELETE")).toBe(false);
    await user.click(dialog.getByRole("button", { name: "Delete CRM" }));

    expect(await screen.findByText("Now at /w/ws1/sources")).toBeInTheDocument();
    expect(calls.some((call) => call.method === "DELETE")).toBe(true);
  });

  it("keeps the confirmation open and says why when the delete fails", async () => {
    mockApi({
      ...ADMIN,
      "DELETE /api/v1/workspaces/ws1/sources/src-1": () => problem(500, "Internal Server Error", "internal error"),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Delete" }));
    await user.click(within(screen.getByRole("dialog", { name: "Delete CRM?" })).getByRole("button", { name: "Delete CRM" }));

    expect(await within(screen.getByRole("dialog")).findByText("Internal Server Error")).toBeInTheDocument();
  });

  it("refuses an incremental stream without a cursor field in the stream dialog", async () => {
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/sources/src-1/streams": () => json([aStream({ cursor_field: null })]),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Set up stream_1" }));
    const dialog = within(screen.getByRole("dialog", { name: "Set up stream_1" }));
    await user.selectOptions(dialog.getByLabelText("Cursor field"), "");
    await user.selectOptions(dialog.getByLabelText("Sync mode"), "incremental");

    expect(dialog.getByText("Choose a cursor field for an incremental stream.")).toBeInTheDocument();
    expect(dialog.getByRole("button", { name: "Save stream" })).toBeDisabled();
  });

  it("edits and clears the delete signal, the reconciliation cron and the sweep limit", async () => {
    const mapped = aStream({
      enabled: true,
      sync_mode: "incremental",
      mapping: {
        key_column: "id",
        columns: [{ column: "email", target: { type: "ignore" }, transforms: [] }],
        delete_when: { column: "updated_at", rule: "present" },
      },
      reconcile_schedule: "0 3 * * *",
      sweep_limit: 0.25,
    });
    const calls = mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/sources/src-1/streams": () => json([mapped]),
      "PUT /api/v1/workspaces/ws1/sources/src-1/streams/stream_1": () => json(mapped),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Set up stream_1" }));
    const dialog = within(screen.getByRole("dialog", { name: "Set up stream_1" }));
    expect(dialog.getByLabelText("Deleted column")).toHaveValue("updated_at");
    expect(dialog.getByLabelText("A record is deleted when the column")).toHaveValue("present");
    expect(dialog.getByLabelText("Reconciliation schedule")).toHaveValue("0 3 * * *");
    expect(dialog.getByLabelText("Largest share a run may remove (%)")).toHaveValue(25);
    await user.selectOptions(dialog.getByLabelText("A record is deleted when the column"), "truthy");
    await user.selectOptions(dialog.getByLabelText("Deleted column"), "name");
    await user.clear(dialog.getByLabelText("Largest share a run may remove (%)"));
    await user.type(dialog.getByLabelText("Largest share a run may remove (%)"), "100");
    await user.click(dialog.getByRole("button", { name: "Save stream" }));

    await waitFor(() => expect(calls.filter((call) => call.method === "PUT")).toHaveLength(1));
    expect(calls.find((call) => call.method === "PUT")!.body).toMatchObject({
      reconcile_schedule: "0 3 * * *",
      sweep_limit: 1,
      mapping: { delete_when: { column: "name", rule: "truthy" }, key_column: "id" },
    });

    // Clearing both sends no delete signal and no cron.
    await user.click(await screen.findByRole("button", { name: "Set up stream_1" }));
    const again = within(screen.getByRole("dialog", { name: "Set up stream_1" }));
    await user.selectOptions(again.getByLabelText("Deleted column"), "");
    await user.clear(again.getByLabelText("Reconciliation schedule"));
    await user.click(again.getByRole("button", { name: "Save stream" }));
    await waitFor(() => expect(calls.filter((call) => call.method === "PUT")).toHaveLength(2));
    const cleared = calls.filter((call) => call.method === "PUT")[1]!;
    expect(cleared.body).toMatchObject({ reconcile_schedule: null, sweep_limit: 0.25 });
    expect((cleared.body as { mapping: object }).mapping).not.toHaveProperty("delete_when");
  });

  it("hides the cron for a full refresh stream and says what a sweep does", async () => {
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/sources/src-1/streams": () => json([aStream()]),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Set up stream_1" }));
    const dialog = within(screen.getByRole("dialog", { name: "Set up stream_1" }));

    expect(dialog.queryByLabelText("Reconciliation schedule")).not.toBeInTheDocument();
    expect(dialog.getByText(/removed after each successful run/)).toBeInTheDocument();
    expect(dialog.getByText(/Set up a mapping for this stream/)).toBeInTheDocument();
    await user.selectOptions(dialog.getByLabelText("Sync mode"), "incremental");
    expect(dialog.getByLabelText("Reconciliation schedule")).toBeInTheDocument();
  });

  it("refuses a sweep limit outside 0 to 100 percent and shows the refusal of the server", async () => {
    mockApi({
      ...ADMIN,
      "PUT /api/v1/workspaces/ws1/sources/src-1/streams/stream_1": () =>
        problem(422, "Unprocessable Entity", "only an incremental stream can be reconciled"),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Set up stream_1" }));
    const dialog = within(screen.getByRole("dialog", { name: "Set up stream_1" }));
    const limit = dialog.getByLabelText("Largest share a run may remove (%)");
    await user.clear(limit);
    await user.type(limit, "150");
    expect(dialog.getByText("Use a number above 0 and up to 100.")).toBeInTheDocument();
    expect(dialog.getByRole("button", { name: "Save stream" })).toBeDisabled();
    await user.clear(limit);
    await user.type(limit, "50");
    await user.click(dialog.getByRole("button", { name: "Save stream" }));

    expect(await dialog.findByText(/only an incremental stream can be reconciled/)).toBeInTheDocument();
  });
});
