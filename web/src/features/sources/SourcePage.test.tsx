import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes, useLocation } from "react-router";
import { afterEach, describe, expect, it, vi } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { SourcePage } from "./SourcePage";
import { aCredentialHolding, aFreshness, aSource, aStream, FAKE_KIND, HTTP_KIND } from "./testData";

function members(role: "admin" | "editor", userId: string) {
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
  "GET /api/v1/workspaces/ws1/credentials": () =>
    json([aCredentialHolding("cred-1", "CRM token", ["token"]), aCredentialHolding("cred-2", "Other CRM token", ["token"])]),
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

  it("shows the freshness of the source and of each stream with when it last succeeded", async () => {
    const hoursAgo = (hours: number) => new Date(Date.now() - hours * 3600_000).toISOString();
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/sources/src-1": () => json(aSource({ freshness: "failing" })),
      "GET /api/v1/workspaces/ws1/sources/src-1/streams": () =>
        json([
          aStream({
            id: "a",
            name: "stream_a",
            enabled: true,
            freshness: aFreshness({
              state: "fresh",
              last_success_at: hoursAgo(3),
              threshold_seconds: 7200,
              threshold_is_default: true,
            }),
          }),
          aStream({
            id: "b",
            name: "stream_b",
            enabled: true,
            staleness_threshold_seconds: 60,
            freshness: aFreshness({ state: "stale", last_success_at: null, threshold_seconds: 60 }),
          }),
          aStream({
            id: "c",
            name: "stream_c",
            enabled: true,
            freshness: aFreshness({ state: "failing", last_success_at: hoursAgo(30) }),
          }),
          aStream({ id: "d", name: "stream_d", enabled: false, freshness: null }),
        ]),
    });
    mount();

    const settings = within(await screen.findByRole("region", { name: "Settings" }));
    expect(settings.getByText("Failing")).toBeInTheDocument();
    const table = await screen.findByRole("table", { name: "Streams of this source" });
    const row = (name: string) => within(within(table).getByText(name).closest("tr")!);
    expect(row("stream_a").getByText("Fresh")).toBeInTheDocument();
    expect(row("stream_a").getByText("3 hours ago")).toBeInTheDocument();
    expect(row("stream_a").getByText("7200 (default)")).toBeInTheDocument();
    expect(row("stream_b").getByText("Stale")).toBeInTheDocument();
    expect(row("stream_b").getByText("Never")).toBeInTheDocument();
    expect(row("stream_b").getByText("60")).toBeInTheDocument();
    expect(row("stream_c").getByText("Failing")).toBeInTheDocument();
    expect(row("stream_c").getByText("1 day ago")).toBeInTheDocument();
    expect(row("stream_c").getByText("None")).toBeInTheDocument();
    expect(row("stream_d").queryByText(/Fresh|Stale|Failing/)).not.toBeInTheDocument();
    expect(row("stream_d").getByText("Not read")).toBeInTheDocument();
    expect(row("stream_d").getByText("—")).toBeInTheDocument();
  });

  it("shows the time since the last success and the threshold in the stream dialog", async () => {
    const threeHoursAgo = new Date(Date.now() - 3 * 3600_000).toISOString();
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/sources/src-1/streams": () =>
        json([
          aStream({
            enabled: true,
            freshness: aFreshness({
              state: "stale",
              last_success_at: threeHoursAgo,
              threshold_seconds: 7200,
              threshold_is_default: true,
            }),
          }),
        ]),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Set up stream_1" }));

    const dialog = within(screen.getByRole("dialog", { name: "Set up stream_1" }));
    expect(dialog.getByText("Stale")).toBeInTheDocument();
    expect(dialog.getByText("3 hours ago")).toBeInTheDocument();
    expect(dialog.getByText("2.0 h (default)")).toBeInTheDocument();
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
      "GET /api/v1/workspaces/ws1/members": () => json(members("editor", MEMBER.id)),
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

  it("shows the credential the source uses, never its secret, and changes it only when another is chosen", async () => {
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
    expect(await dialog.findByLabelText("Credential")).toHaveValue("cred-1");
    await user.selectOptions(dialog.getByLabelText("Credential"), "cred-2");
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
      credential_id: "cred-2",
    });
  });

  it("leaves the credential out of the change when the same one stays chosen", async () => {
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
    expect(patch?.body).not.toHaveProperty("credential_id");
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

  it("edits and clears the reconciliation cron and the sweep limit, and saves the mapping unchanged", async () => {
    const mapped = aStream({
      enabled: true,
      sync_mode: "incremental",
      mapping: {
        key_column: "id",
        columns: [
          { column: "email", target: { type: "ignore" }, transforms: [] },
          { column: "name", target: { type: "list", list_id: "22222222-2222-7222-8222-222222222222" }, transforms: [] },
        ],
        event: { name: "order_placed", properties: [{ property: "sku", column: "name", transforms: [] }] },
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
    expect(dialog.queryByLabelText("Deleted column")).not.toBeInTheDocument();
    expect(dialog.getByText(/Change it under Map/)).toBeInTheDocument();
    expect(dialog.getByLabelText("Reconciliation schedule")).toHaveValue("0 3 * * *");
    expect(dialog.getByLabelText("Largest share a run may remove (%)")).toHaveValue(25);
    await user.clear(dialog.getByLabelText("Largest share a run may remove (%)"));
    await user.type(dialog.getByLabelText("Largest share a run may remove (%)"), "100");
    await user.click(dialog.getByRole("button", { name: "Save stream" }));

    await waitFor(() => expect(calls.filter((call) => call.method === "PUT")).toHaveLength(1));
    const first = calls.find((call) => call.method === "PUT")!;
    expect(first.body).toMatchObject({ reconcile_schedule: "0 3 * * *", sweep_limit: 1 });
    // The mapping goes back exactly as it was read: the event, the list, the ignored column, the
    // key and the delete signal.
    expect(JSON.stringify((first.body as { mapping: unknown }).mapping)).toBe(JSON.stringify(mapped.mapping));

    // Clearing the cron sends none.
    await user.click(await screen.findByRole("button", { name: "Set up stream_1" }));
    const again = within(screen.getByRole("dialog", { name: "Set up stream_1" }));
    await user.clear(again.getByLabelText("Reconciliation schedule"));
    await user.click(again.getByRole("button", { name: "Save stream" }));
    await waitFor(() => expect(calls.filter((call) => call.method === "PUT")).toHaveLength(2));
    const cleared = calls.filter((call) => call.method === "PUT")[1]!;
    expect(cleared.body).toMatchObject({ reconcile_schedule: null, sweep_limit: 0.25 });
    expect(JSON.stringify((cleared.body as { mapping: unknown }).mapping)).toBe(JSON.stringify(mapped.mapping));
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

describe("connecting an account", () => {
  const original = window.location;
  afterEach(() => {
    Object.defineProperty(window, "location", { value: original, configurable: true });
  });

  const MANIFEST_SOURCE = (connected: boolean) =>
    aSource({
      kind: "http",
      name: "API",
      config: { manifest: { version: "v1", streams: [{ name: "a" }, { name: "b" }] } },
      credential_fields_set: ["client_id"],
      oauth: {
        required: true,
        connected,
        connected_at: connected ? "2026-10-04T09:00:00Z" : null,
      },
    });

  function connectable(connected: boolean) {
    return {
      ...ADMIN,
      "GET /api/v1/source-kinds": () => json([HTTP_KIND]),
      "GET /api/v1/workspaces/ws1/sources/src-1": () => json(MANIFEST_SOURCE(connected)),
    };
  }

  function mountAt(search: string) {
    return renderRoute(
      `/w/ws1/sources/src-1${search}`,
      "/w/:ws/sources/*",
      <Routes>
        <Route
          path=":sourceId"
          element={
            <>
              <SourcePage />
              <Where />
            </>
          }
        />
      </Routes>,
    );
  }

  function Where() {
    const location = useLocation();
    return <p>Address {location.pathname + location.search}</p>;
  }

  it("shows a Connect account button, sends the browser to the provider and does nothing else", async () => {
    const assign = vi.fn();
    Object.defineProperty(window, "location", { value: { assign }, configurable: true });
    const calls = mockApi({
      ...connectable(false),
      "POST /api/v1/workspaces/ws1/sources/src-1/oauth/start": () =>
        json({ authorization_url: "https://auth.example.test/authorize?state=s" }),
    });
    const user = userEvent.setup();
    mountAt("");

    expect(await screen.findByText("Not connected")).toBeInTheDocument();
    expect(screen.getByText("2 streams described")).toBeInTheDocument();
    await user.click(await screen.findByRole("button", { name: "Connect account" }));

    await waitFor(() => expect(assign).toHaveBeenCalledWith("https://auth.example.test/authorize?state=s"));
    expect(calls.find((call) => call.path.endsWith("/oauth/start"))?.headers.get("x-csrf-token")).toBe("csrf-source");
  });

  it("offers Reconnect and shows when the account was connected", async () => {
    mockApi(connectable(true));
    mountAt("");

    expect(await screen.findByRole("button", { name: "Reconnect account" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Connect account" })).not.toBeInTheDocument();
    expect(screen.getByText(/^Connected /)).toBeInTheDocument();
  });

  it("shows why starting failed", async () => {
    mockApi({
      ...connectable(false),
      "POST /api/v1/workspaces/ws1/sources/src-1/oauth/start": () =>
        problem(409, "Conflict", "connecting an account needs server.public_url to be set"),
    });
    const user = userEvent.setup();
    mountAt("");

    await user.click(await screen.findByRole("button", { name: "Connect account" }));

    expect(await screen.findByText(/needs server.public_url/)).toBeInTheDocument();
  });

  it("shows a success banner once and takes the outcome out of the address", async () => {
    mockApi(connectable(true));
    mountAt("?oauth=connected");

    expect(await screen.findByText(/The account is connected/)).toBeInTheDocument();
    await waitFor(() => expect(screen.getByText("Address /w/ws1/sources/src-1")).toBeInTheDocument());
  });

  it("explains each failure in plain words", async () => {
    for (const [reason, text] of [
      ["state_mismatch", /different person or session/],
      ["no_refresh_token", /did not give a refresh token/],
      ["unknown_state", /expired or was already used/],
    ] as const) {
      mockApi(connectable(false));
      const view = mountAt(`?oauth=failed&reason=${reason}`);

      expect(await screen.findByText("Connecting the account failed")).toBeInTheDocument();
      expect(screen.getByText(text)).toBeInTheDocument();
      await waitFor(() => expect(screen.getByText("Address /w/ws1/sources/src-1")).toBeInTheDocument());
      view.unmount();
    }
  });

  it("gives a member no Connect button and no banner action", async () => {
    mockApi({
      ...connectable(false),
      "GET /api/v1/session": () => json(session(MEMBER, "csrf-m")),
      "GET /api/v1/workspaces/ws1/members": () => json(members("editor", MEMBER.id)),
    });
    mountAt("");

    expect(await screen.findByText("Not connected")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Connect account" })).not.toBeInTheDocument();
  });

  it("shows no account line for a source that needs none", async () => {
    mockApi(ADMIN);
    mountAt("");

    await screen.findByRole("heading", { name: "CRM" });
    expect(screen.queryByText("Account")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Connect account" })).not.toBeInTheDocument();
  });
});
