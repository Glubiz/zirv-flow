import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { hoursOf, windowOf } from "./model";
import { SendingSettingsPage } from "./SendingSettingsPage";

const WS = "/api/v1/workspaces/ws1";

const UTM_OFF = { enabled: false, source: "{workspace}", medium: "email", campaign: "{flow}", content: "{template}", domains: [] as string[] };

const STORED = {
  frequency_caps: [{ channel: "email", window: "P1D", limit: 3, mode: "skip" }],
  quiet_hours: { start: "21:00", end: "08:00", days: ["mon", "tue", "wed", "thu", "fri"] },
  revision: 4,
};

function access(role: "admin" | "member"): Record<string, Handler> {
  const who = role === "admin" ? OWNER : MEMBER;
  return {
    "GET /api/v1/session": () => json(session(who)),
    "GET /api/v1/workspaces/ws1/members": () => json([{ user_id: who.id, email: who.email, role, added_at: "2026-10-01T09:00:00Z" }]),
    [`GET ${WS}`]: () => json({ id: "ws1", name: "Spring campaigns", slug: "spring", timezone: "Europe/Copenhagen", utm: UTM_OFF, created_at: "2026-10-01T09:00:00Z" }),
    [`GET ${WS}/sending-domains`]: () => json([]),
  };
}

function mount() {
  return renderRoute("/w/ws1/settings/sending", "/w/:ws/settings/sending", <SendingSettingsPage />);
}

describe("sending limits", () => {
  it("shows the stored caps and quiet hours with the zone note", async () => {
    mockApi({ ...access("admin"), [`GET ${WS}/sending/settings`]: () => json(STORED) });
    mount();

    const cap = await screen.findByRole("group", { name: "Cap 1" });
    expect(within(cap).getByLabelText("Window of cap 1 in hours")).toHaveValue(24);
    expect(within(cap).getByLabelText("Limit of cap 1")).toHaveValue(3);
    expect(within(cap).getByLabelText("Over the limit for cap 1")).toHaveValue("skip");
    expect(screen.getByLabelText("Quiet period starts")).toHaveValue("21:00");
    expect(screen.getByLabelText("Sat")).not.toBeChecked();
    expect(screen.getByLabelText("Mon")).toBeChecked();
    expect(screen.getByText(/otherwise the workspace zone \(Europe\/Copenhagen\)/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save limits" })).toBeDisabled();
  });

  it("replaces the whole document with the edited caps and quiet hours", async () => {
    const calls = mockApi({
      ...access("admin"),
      [`GET ${WS}/sending/settings`]: () => json(STORED),
      [`PUT ${WS}/sending/settings`]: async (request) => json(await request.json()),
    });
    const user = userEvent.setup();
    mount();

    const cap = await screen.findByRole("group", { name: "Cap 1" });
    const hours = within(cap).getByLabelText("Window of cap 1 in hours");
    await user.clear(hours);
    await user.type(hours, "12");
    await user.selectOptions(within(cap).getByLabelText("Over the limit for cap 1"), "defer");
    await user.click(screen.getByRole("button", { name: "Add a cap" }));
    const added = screen.getByRole("group", { name: "Cap 2" });
    await user.clear(within(added).getByLabelText("Limit of cap 2"));
    await user.type(within(added).getByLabelText("Limit of cap 2"), "10");
    await user.click(screen.getByLabelText("Fri"));
    await user.click(screen.getByRole("button", { name: "Save limits" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true));
    expect(calls.find((call) => call.method === "PUT")?.body).toEqual({
      frequency_caps: [
        { channel: "email", window: "PT12H", limit: 3, mode: "defer" },
        { channel: "email", window: "P1D", limit: 10, mode: "skip" },
      ],
      quiet_hours: { start: "21:00", end: "08:00", days: ["mon", "tue", "wed", "thu"] },
      revision: 4,
    });
    expect(await screen.findByText(/Saved\./)).toBeInTheDocument();
  });

  it("keeps the saved notice when the save reloads the settings it stored", async () => {
    let stored: unknown = STORED;
    mockApi({
      ...access("admin"),
      [`GET ${WS}/sending/settings`]: () => json(stored),
      [`PUT ${WS}/sending/settings`]: async (request) => {
        const body = (await request.json()) as { revision: number };
        stored = { ...body, revision: body.revision + 1 };
        return json(stored);
      },
    });
    const user = userEvent.setup();
    mount();

    const limit = await screen.findByLabelText("Limit of cap 1");
    await user.clear(limit);
    await user.type(limit, "5");
    await user.click(screen.getByRole("button", { name: "Save limits" }));

    expect(await screen.findByText(/Saved\./)).toBeInTheDocument();
    // The reload brought back what was stored, and the form shows it.
    await waitFor(() => expect(screen.getByLabelText("Limit of cap 1")).toHaveValue(5));
    expect(screen.getByText(/Saved\./)).toBeInTheDocument();
  });

  it("clears the saved notice when the form is edited again", async () => {
    let stored: unknown = STORED;
    mockApi({
      ...access("admin"),
      [`GET ${WS}/sending/settings`]: () => json(stored),
      [`PUT ${WS}/sending/settings`]: async (request) => {
        const body = (await request.json()) as { revision: number };
        stored = { ...body, revision: body.revision + 1 };
        return json(stored);
      },
    });
    const user = userEvent.setup();
    mount();

    const limit = await screen.findByLabelText("Limit of cap 1");
    await user.clear(limit);
    await user.type(limit, "5");
    await user.click(screen.getByRole("button", { name: "Save limits" }));
    expect(await screen.findByText(/Saved\./)).toBeInTheDocument();
    await waitFor(() => expect(screen.getByLabelText("Limit of cap 1")).toHaveValue(5));

    await user.type(screen.getByLabelText("Limit of cap 1"), "0");
    expect(screen.queryByText(/Saved\./)).not.toBeInTheDocument();
  });

  it("keeps the draft and offers the latest settings when they changed since the page was opened", async () => {
    let stored: unknown = STORED;
    let stale = true;
    const calls = mockApi({
      ...access("admin"),
      [`GET ${WS}/sending/settings`]: () => json(stored),
      [`PUT ${WS}/sending/settings`]: () => {
        if (stale) {
          stored = { frequency_caps: [], quiet_hours: null, revision: 5 };
          return problem(409, "Conflict", "the send settings are not at the revision named", undefined, "send_settings_revision_mismatch");
        }
        return json(stored);
      },
    });
    const user = userEvent.setup();
    mount();

    const limit = await screen.findByLabelText("Limit of cap 1");
    await user.clear(limit);
    await user.type(limit, "9");
    await user.click(screen.getByRole("button", { name: "Save limits" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(/changed since this page was opened/);
    expect(screen.getByLabelText("Limit of cap 1")).toHaveValue(9);
    expect(screen.queryByText(/Saved\./)).not.toBeInTheDocument();
    expect(calls.find((call) => call.method === "PUT")?.body).toMatchObject({ revision: 4 });

    stale = false;
    await user.click(within(alert).getByRole("button", { name: "Load the latest limits" }));
    expect(await screen.findByText(/No caps\./)).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(screen.getByLabelText("Hold messages during quiet hours")).not.toBeChecked();
  });

  it("removes the quiet hours and every cap by sending nothing for them", async () => {
    const calls = mockApi({
      ...access("admin"),
      [`GET ${WS}/sending/settings`]: () => json(STORED),
      [`PUT ${WS}/sending/settings`]: async (request) => json(await request.json()),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: /Remove cap 1/ }));
    await user.click(screen.getByLabelText("Hold messages during quiet hours"));
    await user.click(screen.getByRole("button", { name: "Save limits" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true));
    expect(calls.find((call) => call.method === "PUT")?.body).toEqual({ frequency_caps: [], quiet_hours: null, revision: 4 });
  });

  it("explains what is wrong before sending and sends nothing", async () => {
    const calls = mockApi({
      ...access("admin"),
      [`GET ${WS}/sending/settings`]: () => json(STORED),
      [`PUT ${WS}/sending/settings`]: async (request) => json(await request.json()),
    });
    const user = userEvent.setup();
    mount();

    const cap = await screen.findByRole("group", { name: "Cap 1" });
    const hours = within(cap).getByLabelText("Window of cap 1 in hours");
    await user.clear(hours);
    await user.type(hours, "800");
    await user.clear(screen.getByLabelText("Quiet period ends"));
    await user.type(screen.getByLabelText("Quiet period ends"), "21:00");
    await user.click(screen.getByRole("button", { name: "Save limits" }));

    expect(await screen.findByText(/from 1 to 744/)).toBeInTheDocument();
    expect(screen.getByText("The end has to differ from the start.")).toBeInTheDocument();
    expect(calls.some((call) => call.method === "PUT")).toBe(false);
  });

  it("shows what the server refused", async () => {
    mockApi({
      ...access("admin"),
      [`GET ${WS}/sending/settings`]: () => json(STORED),
      [`PUT ${WS}/sending/settings`]: () => problem(422, "Unprocessable Entity", "the send settings are not valid"),
    });
    const user = userEvent.setup();
    mount();

    const limit = await screen.findByLabelText("Limit of cap 1");
    await user.type(limit, "0");
    await user.click(screen.getByRole("button", { name: "Save limits" }));

    expect(await screen.findByText("the send settings are not valid")).toBeInTheDocument();
  });

  it("is read-only for a member who is not an admin", async () => {
    mockApi({ ...access("member"), [`GET ${WS}/sending/settings`]: () => json(STORED) });
    mount();

    expect(await screen.findByLabelText("Limit of cap 1")).toBeDisabled();
    expect(screen.queryByRole("button", { name: "Save limits" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add a cap" })).not.toBeInTheDocument();
    expect(screen.getByText(/Only workspace admins and the instance owner can change/)).toBeInTheDocument();
  });

  it("shows an empty state for no caps and no quiet hours, and an error with a retry", async () => {
    let fail = true;
    mockApi({
      ...access("admin"),
      [`GET ${WS}/sending/settings`]: () => (fail ? problem(500, "Internal Server Error", "internal error") : json({ frequency_caps: [], quiet_hours: null, revision: 0 })),
    });
    const user = userEvent.setup();
    mount();

    const retry = await screen.findByRole("button", { name: /try again/i });
    fail = false;
    await user.click(retry);
    expect(await screen.findByText(/No caps\./)).toBeInTheDocument();
    expect(screen.getByLabelText("Hold messages during quiet hours")).not.toBeChecked();
  });
});

describe("window text", () => {
  it("reads and writes whole hours and days", () => {
    expect(hoursOf("P1D")).toBe(24);
    expect(hoursOf("PT12H")).toBe(12);
    expect(hoursOf("P1DT6H")).toBe(30);
    expect(hoursOf("P1W")).toBeNull();
    expect(windowOf(48)).toBe("P2D");
    expect(windowOf(30)).toBe("PT30H");
  });
});
