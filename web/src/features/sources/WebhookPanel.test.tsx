import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes, useLocation } from "react-router";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { SourcePage } from "./SourcePage";
import { aFreshness, aSource, aStream, aWebhook, FAKE_KIND, WEBHOOK_KIND } from "./testData";

function Landing() {
  const location = useLocation();
  return <p>Now at {location.pathname}</p>;
}

function mount() {
  return renderRoute(
    "/w/ws1/sources/src-hook",
    "/w/:ws/sources/*",
    <Routes>
      <Route path=":sourceId" element={<SourcePage />} />
      <Route index element={<Landing />} />
    </Routes>,
  );
}

function members(role: "admin" | "editor", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

const HOOK_SOURCE = aSource({ id: "src-hook", kind: "webhook", name: "Orders", config: {}, credential_fields_set: [] });

function payloadStream(change: Parameters<typeof aStream>[0] = {}) {
  return aStream({
    id: "str-hook",
    name: "payload",
    enabled: true,
    mapping: { columns: [] },
    primary_key: [],
    discovered: { fields: [], sync_modes: ["full_refresh"], default_cursor_field: null, default_primary_key: [] },
    ...change,
  });
}

const ADMIN = {
  "GET /api/v1/session": () => json(session(OWNER, "csrf-hook")),
  "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
  "GET /api/v1/source-kinds": () => json([FAKE_KIND, WEBHOOK_KIND]),
  "GET /api/v1/workspaces/ws1/sources/src-hook": () => json(HOOK_SOURCE),
  "GET /api/v1/workspaces/ws1/sources/src-hook/streams": () => json([payloadStream()]),
  "GET /api/v1/workspaces/ws1/webhooks": () => json([aWebhook()]),
};

describe("a webhook source", () => {
  it("shows its URL, signature, freshness and last received time, with no run controls", async () => {
    const hoursAgo = new Date(Date.now() - 2 * 3600_000).toISOString();
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/sources/src-hook/streams": () =>
        json([
          payloadStream({
            freshness: aFreshness({ state: "stale", last_success_at: hoursAgo, threshold_seconds: 3600 }),
          }),
        ]),
    });
    mount();

    const panel = within(await screen.findByRole("region", { name: "Webhook" }));
    expect(await panel.findByText(`${window.location.origin}/hooks/hook-1`)).toBeInTheDocument();
    expect(panel.getByRole("button", { name: "Copy the URL" })).toBeInTheDocument();
    expect(panel.getByText("Receiving")).toBeInTheDocument();
    expect(panel.getByText("Stale")).toBeInTheDocument();
    expect(panel.getByText("2 hours ago")).toBeInTheDocument();
    expect(panel.getByText(/HMAC-SHA256 of "timestamp.body" in X-Zirv-Signature/)).toBeInTheDocument();
    expect(panel.getByText("id")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Test connection" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Run now" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Discover streams/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("region", { name: "Settings" })).not.toBeInTheDocument();
    expect(screen.getByText("A webhook has no runs")).toBeInTheDocument();
  });

  it("says why it is not receiving and where to fix it", async () => {
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/webhooks": () => json([aWebhook({ receiving: false })]),
    });
    mount();

    const panel = within(await screen.findByRole("region", { name: "Webhook" }));
    expect(await panel.findByText(/Not receiving: turn on the stream and set a mapping/)).toBeInTheDocument();
    expect(panel.getByText("Not receiving", { selector: ".pill, span" })).toBeInTheDocument();
  });

  it("shows the empty state when the hook row is gone", async () => {
    mockApi({ ...ADMIN, "GET /api/v1/workspaces/ws1/webhooks": () => json([]) });
    mount();

    expect(await screen.findByText("This source has no webhook")).toBeInTheDocument();
  });

  it("shows the error when the webhook cannot be loaded", async () => {
    mockApi({ ...ADMIN, "GET /api/v1/workspaces/ws1/webhooks": () => problem(500, "Internal", "internal error") });
    mount();

    expect(await screen.findByText("internal error")).toBeInTheDocument();
  });

  it("rotates the secret after a confirmation and shows the new one once", async () => {
    const calls = mockApi({
      ...ADMIN,
      "POST /api/v1/workspaces/ws1/webhooks/hook-1/rotate-secret": () => json({ secret: "zfwh_fresh" }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Rotate secret" }));
    const confirm = within(screen.getByRole("dialog", { name: "Rotate the secret of Orders?" }));
    expect(confirm.getByText(/stops working at once/)).toBeInTheDocument();
    expect(calls.some((call) => call.path.endsWith("/rotate-secret"))).toBe(false);
    await user.click(confirm.getByRole("button", { name: "Rotate secret" }));

    const shown = await screen.findByRole("dialog", { name: "New secret" });
    expect(shown).toHaveTextContent("zfwh_fresh");
    const rotated = calls.find((call) => call.path.endsWith("/rotate-secret"));
    expect(rotated?.method).toBe("POST");
    expect(rotated?.headers.get("x-csrf-token")).toBe("csrf-hook");
    await user.click(within(shown).getByRole("button", { name: "I have saved the secret" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(screen.queryByText("zfwh_fresh")).not.toBeInTheDocument();
  });

  it("keeps the secret when the rotation is cancelled", async () => {
    const calls = mockApi(ADMIN);
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Rotate secret" }));
    await user.click(screen.getByRole("button", { name: "Keep the secret" }));

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(calls.some((call) => call.method === "POST")).toBe(false);
  });

  it("deletes the webhook after a confirmation and goes back to the sources", async () => {
    const calls = mockApi({
      ...ADMIN,
      "DELETE /api/v1/workspaces/ws1/webhooks/hook-1": () => new Response(null, { status: 204 }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Delete webhook" }));
    const confirm = within(screen.getByRole("dialog", { name: "Delete Orders?" }));
    expect(calls.some((call) => call.method === "DELETE")).toBe(false);
    await user.click(confirm.getByRole("button", { name: "Delete Orders" }));

    expect(await screen.findByText("Now at /w/ws1/sources")).toBeInTheDocument();
    expect(calls.some((call) => call.method === "DELETE" && call.path.endsWith("/webhooks/hook-1"))).toBe(true);
  });

  it("edits the scheme and reloads", async () => {
    const calls = mockApi({
      ...ADMIN,
      "PATCH /api/v1/workspaces/ws1/webhooks/hook-1": () => json(aWebhook({ tolerance_seconds: 60 })),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Edit webhook" }));
    const dialog = within(screen.getByRole("dialog", { name: "Edit Orders" }));
    const tolerance = dialog.getByLabelText("Tolerance (seconds)");
    await user.clear(tolerance);
    await user.type(tolerance, "60");
    await user.click(dialog.getByRole("button", { name: "Save webhook" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.body).toMatchObject({ name: "Orders", tolerance_seconds: 60, timestamp_header: "X-Zirv-Timestamp" });
    expect(await screen.findByText("Saved Orders.")).toBeInTheDocument();
  });

  it("offers a member no change", async () => {
    mockApi({
      ...ADMIN,
      "GET /api/v1/session": () => json(session(MEMBER, "csrf-member")),
      "GET /api/v1/workspaces/ws1/members": () => json(members("editor", MEMBER.id)),
    });
    mount();

    expect(await screen.findByText("You can look at this webhook but not change it.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Rotate secret" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Delete webhook" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Edit webhook" })).not.toBeInTheDocument();
  });

  it("sets up the stream with only whether it receives and its stale time", async () => {
    const calls = mockApi({
      ...ADMIN,
      "PUT /api/v1/workspaces/ws1/sources/src-hook/streams/payload": () => json(payloadStream()),
    });
    const user = userEvent.setup();
    mount();

    const table = await screen.findByRole("table", { name: "Streams of this source" });
    await user.click(within(table).getByRole("button", { name: "Set up payload" }));
    const dialog = within(screen.getByRole("dialog", { name: "Set up payload" }));
    expect(dialog.queryByLabelText("Sync mode")).not.toBeInTheDocument();
    expect(dialog.queryByLabelText("Largest share a run may remove (%)")).not.toBeInTheDocument();
    await user.type(dialog.getByLabelText("Expect a payload at least every (seconds)"), "900");
    await user.click(dialog.getByRole("button", { name: "Save stream" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true));
    const put = calls.find((call) => call.method === "PUT");
    expect(put?.body).toMatchObject({ enabled: true, sync_mode: "full_refresh", primary_key: [], staleness_threshold_seconds: 900 });
  });
});
