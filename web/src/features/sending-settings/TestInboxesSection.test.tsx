import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { SendingSettingsPage } from "./SendingSettingsPage";

const WS = "/api/v1/workspaces/ws1";
const ENTRY = { id: "e1", display_hint: "q***@example.com", created_at: "2026-10-11T09:00:00Z" };

function routes(role: "admin" | "editor" | null, extra: Record<string, Handler> = {}): Record<string, Handler> {
  const who = role === "admin" ? OWNER : MEMBER;
  return {
    "GET /api/v1/session": () => json(session(who)),
    "GET /api/v1/workspaces/ws1/members": () => json(role === null ? [] : [{ user_id: who.id, email: who.email, role, added_at: "2026-10-01T09:00:00Z" }]),
    [`GET ${WS}`]: () =>
      json({
        id: "ws1",
        name: "Spring",
        slug: "spring",
        timezone: "UTC",
        utm: { enabled: false, source: "{workspace}", medium: "email", campaign: "{flow}", content: "{template}", domains: [] },
        created_at: "2026-10-01T09:00:00Z",
      }),
    [`GET ${WS}/sending/settings`]: () => json({ frequency_caps: [], quiet_hours: null, revision: 1 }),
    [`GET ${WS}/sending-domains`]: () => json([]),
    [`GET ${WS}/test-send-allowlist`]: () => json({ entries: [ENTRY] }),
    [`GET ${WS}/test-send-allowlist/options`]: () => json([{ id: "e1", name: "q***@example.com" }]),
    ...extra,
  };
}

function mount() {
  return renderRoute("/w/ws1/settings/sending", "/w/:ws/settings/sending", <SendingSettingsPage />);
}

async function section() {
  const heading = await screen.findByRole("heading", { name: "Test inboxes" });
  const root = heading.closest("#test-inboxes");
  expect(root).not.toBeNull();
  return within(root as HTMLElement);
}

describe("the test inboxes section", () => {
  it("lists the entries by hint and when they were added, under the anchor", async () => {
    mockApi(routes("admin"));
    mount();

    const inboxes = await section();

    const row = await inboxes.findByRole("listitem");
    expect(row).toHaveTextContent("q***@example.com");
    expect(row).toHaveTextContent(/Added /);
  });

  it("says so when there are none", async () => {
    mockApi(routes("admin", { [`GET ${WS}/test-send-allowlist`]: () => json({ entries: [] }) }));
    mount();

    expect(await (await section()).findByText("No test inboxes yet.")).toBeInTheDocument();
  });

  it("shows an error with a retry", async () => {
    let asked = 0;
    mockApi(
      routes("admin", {
        [`GET ${WS}/test-send-allowlist`]: () => {
          asked += 1;
          return asked === 1 ? problem(500, "Internal Server Error", "internal error") : json({ entries: [] });
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await (await section()).findByRole("button", { name: "Try again" }));

    expect(await (await section()).findByText("No test inboxes yet.")).toBeInTheDocument();
  });

  it("adds an address and shows only the masked entry afterwards", async () => {
    let entries = [] as (typeof ENTRY)[];
    const calls = mockApi(
      routes("admin", {
        [`GET ${WS}/test-send-allowlist`]: () => json({ entries }),
        [`POST ${WS}/test-send-allowlist`]: () => {
          entries = [ENTRY];
          return json(ENTRY, 201);
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    const inboxes = await section();
    await user.type(await inboxes.findByLabelText("Address"), "qa.person@example.com");
    await user.click(inboxes.getByRole("button", { name: "Add address" }));

    expect(await inboxes.findByRole("listitem")).toHaveTextContent("q***@example.com");
    expect(calls.filter((call) => call.method === "POST").map((call) => call.body)).toEqual([{ address: "qa.person@example.com" }]);
    expect(inboxes.getByLabelText("Address")).toHaveValue("");
    expect(document.body.textContent).not.toContain("qa.person@example.com");
  });

  it("says why a duplicate or malformed address is refused", async () => {
    let status = 409;
    mockApi(
      routes("admin", {
        [`POST ${WS}/test-send-allowlist`]: () => problem(status, "Refused", "server words"),
      }),
    );
    const user = userEvent.setup();
    mount();

    const inboxes = await section();
    await user.type(await inboxes.findByLabelText("Address"), "dup@example.com");
    await user.click(inboxes.getByRole("button", { name: "Add address" }));
    expect(await inboxes.findByText("That address is already a test inbox.")).toBeInTheDocument();

    status = 422;
    await user.click(inboxes.getByRole("button", { name: "Add address" }));
    expect(await inboxes.findByText("That is not a valid email address.")).toBeInTheDocument();
  });

  it("removes an entry after a confirm", async () => {
    let entries = [ENTRY];
    const calls = mockApi(
      routes("admin", {
        [`GET ${WS}/test-send-allowlist`]: () => json({ entries }),
        [`DELETE ${WS}/test-send-allowlist/e1`]: () => {
          entries = [];
          return new Response(null, { status: 204 });
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    const inboxes = await section();
    await user.click(await inboxes.findByRole("button", { name: /Remove q\*\*\*@example.com/ }));
    expect(calls.some((call) => call.method === "DELETE")).toBe(false);
    const dialog = await screen.findByRole("dialog", { name: "Remove this test inbox?" });
    await user.click(within(dialog).getByRole("button", { name: "Remove" }));

    expect(await inboxes.findByText("No test inboxes yet.")).toBeInTheDocument();
    expect(calls.filter((call) => call.method === "DELETE")).toHaveLength(1);
  });

  it("gives a writer the list read-only, from the options read", async () => {
    const calls = mockApi(routes("editor"));
    mount();

    const inboxes = await section();

    expect(await inboxes.findByRole("listitem")).toHaveTextContent("q***@example.com");
    expect(inboxes.queryByRole("button", { name: /Remove/ })).not.toBeInTheDocument();
    expect(inboxes.queryByLabelText("Address")).not.toBeInTheDocument();
    expect(inboxes.queryByRole("button", { name: "Add address" })).not.toBeInTheDocument();
    expect(calls.map((call) => call.path)).not.toContain(`${WS}/test-send-allowlist`);
    await waitFor(() => expect(inboxes.getByText(/Only workspace admins/)).toBeInTheDocument());
  });
});
