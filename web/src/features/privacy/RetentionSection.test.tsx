import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { PrivacyPage } from "./PrivacyPage";

const RETENTION = "/api/v1/workspaces/ws1/retention";

function win(key: string, value: number | null, extra: Record<string, unknown> = {}) {
  return { key, value, unit: "days", source: "deployment", min: null, max: null, editable: false, last_run: null, ...extra };
}

function windows(messages = 395) {
  return [
    win("events", 760),
    win("messages", messages, { editable: true, min: 30, max: 395, last_run: { ran_at: "2026-10-01T09:00:00Z", deleted: 12 } }),
    win("decision_log", 395, { editable: true, min: 30, max: 395 }),
    win("audit", 730),
    win("ingest_rejects", 30),
    win("import_files", 7),
    win("job_history", 14),
    win("assistant_transcripts", null),
  ];
}

function routes(role: "admin" | "editor", extra: Parameters<typeof mockApi>[0] = {}) {
  return {
    "GET /api/v1/session": () => json(session(role === "admin" ? OWNER : MEMBER)),
    "GET /api/v1/workspaces/ws1/members": () => json([{ user_id: (role === "admin" ? OWNER : MEMBER).id, email: "x@example.test", role, added_at: "2026-10-01T09:00:00Z" }]),
    "GET /api/v1/workspaces/ws1/privacy/erasures": () => json({ erasures: [], next_before: null }),
    "GET /api/v1/workspaces/ws1/privacy/exports": () => json({ exports: [], next_before: null }),
    "GET /api/v1/workspaces/ws1/privacy/settings": () => json({ orphan_policy: "keep" }),
    [`GET ${RETENTION}`]: () => json({ windows: windows() }),
    ...extra,
  };
}

async function open() {
  const user = userEvent.setup();
  renderRoute("/w/ws1/privacy", "/w/:ws/privacy", <PrivacyPage />);
  const table = await screen.findByRole("table", { name: "Retention windows" });
  return { user, table };
}

afterEach(() => vi.unstubAllGlobals());

describe("retention section", () => {
  it("lists the eight windows with the right tags and links", async () => {
    mockApi(routes("admin"));
    const { table } = await open();

    expect(within(table).getAllByRole("row")).toHaveLength(9);
    expect(within(table).getByRole("row", { name: /Events 25 months/ })).toBeInTheDocument();
    expect(within(table).getByRole("row", { name: /Messages 13 months/ })).toBeInTheDocument();
    expect(within(table).getByRole("row", { name: /Assistant transcripts Off/ })).toBeInTheDocument();
    expect(within(table).getAllByText("Deployment")).toHaveLength(4);
    expect(within(table).getByRole("link", { name: "Per-event overrides" })).toHaveAttribute("href", "/w/ws1/events");
    expect(within(table).getByRole("link", { name: "Assistant settings" })).toHaveAttribute("href", "/w/ws1/ai");
  });

  it("offers Shorten on the two editable windows for an admin and on no other", async () => {
    mockApi(routes("admin"));
    const { table } = await open();
    expect(within(table).getAllByRole("button", { name: /^Shorten/ })).toHaveLength(2);
  });

  it("offers no Shorten to someone who cannot administer", async () => {
    mockApi(routes("editor"));
    renderRoute("/w/ws1/privacy", "/w/:ws/privacy", <PrivacyPage />);
    expect(await screen.findByText("Only workspace admins can use this page")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Shorten/ })).not.toBeInTheDocument();
  });

  it("shows the last purge behind the row menu", async () => {
    mockApi(routes("admin"));
    const { user, table } = await open();
    await user.click(within(table).getByRole("button", { name: "Last purge for Messages" }));
    expect(await screen.findByText("12 rows removed")).toBeInTheDocument();
  });

  it("shows the bounds, rejects a value outside them before sending, and converts months to days", async () => {
    const calls = mockApi(routes("admin", { [`PUT ${RETENTION}`]: () => json({ windows: windows(183) }) }));
    const { user, table } = await open();
    await user.click(within(table).getByRole("button", { name: "Shorten Messages" }));

    const dialog = await screen.findByRole("dialog", { name: "Shorten a window" });
    expect(within(dialog).getByText("30 days to 13 months")).toBeInTheDocument();
    const amount = within(dialog).getByLabelText("Keep for");
    await user.clear(amount);
    await user.type(amount, "14");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    expect(await within(dialog).findByText(/Out of range/)).toBeInTheDocument();
    expect(calls.some((call) => call.method === "PUT")).toBe(false);

    await user.clear(amount);
    await user.type(amount, "6");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(calls.find((call) => call.method === "PUT")?.body).toEqual({ messages_days: 183 });
  });

  it("refreshes the row after a save and changes nothing on Cancel", async () => {
    let current = 395;
    mockApi(
      routes("admin", {
        [`GET ${RETENTION}`]: () => json({ windows: windows(current) }),
        [`PUT ${RETENTION}`]: () => {
          current = 90;
          return json({ windows: windows(90) });
        },
      }),
    );
    const { user, table } = await open();
    await user.click(within(table).getByRole("button", { name: "Shorten Messages" }));
    await user.click(await screen.findByRole("button", { name: "Cancel" }));
    expect(within(table).getByRole("row", { name: /Messages 13 months/ })).toBeInTheDocument();

    await user.click(within(table).getByRole("button", { name: "Shorten Messages" }));
    const dialog = await screen.findByRole("dialog");
    await user.selectOptions(within(dialog).getByLabelText("Unit"), "days");
    const amount = within(dialog).getByLabelText("Keep for");
    await user.clear(amount);
    await user.type(amount, "90");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("row", { name: /Messages 90 days/ })).toBeInTheDocument();
  });

  it("shows a server 422 inline and keeps the dialog open", async () => {
    mockApi(
      routes("admin", {
        [`PUT ${RETENTION}`]: () => problem(422, "Unprocessable", "decision days must be within 30 to 395", undefined, "retention_out_of_bounds"),
      }),
    );
    const { user, table } = await open();
    await user.click(within(table).getByRole("button", { name: "Shorten Decision log" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    expect(await within(dialog).findByText(/must be within 30 to 395/)).toBeInTheDocument();
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it.each([
    ["empty", ""],
    ["zero", "0"],
    ["negative", "-5"],
    ["decimal", "2.5"],
    ["very large", "99999999999999999999"],
  ])("refuses %s input before sending", async (_name, value) => {
    const calls = mockApi(routes("admin", { [`PUT ${RETENTION}`]: () => json({ windows: windows() }) }));
    const { user, table } = await open();
    await user.click(within(table).getByRole("button", { name: "Shorten Messages" }));
    const dialog = await screen.findByRole("dialog");
    const amount = within(dialog).getByLabelText("Keep for");
    await user.clear(amount);
    if (value !== "") await user.type(amount, value);
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    expect(await within(dialog).findByText(/Enter a whole number|Out of range/)).toBeInTheDocument();
    expect(calls.some((call) => call.method === "PUT")).toBe(false);
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("keeps the entered value and unit after a 422", async () => {
    mockApi(
      routes("admin", {
        [`PUT ${RETENTION}`]: () => problem(422, "Unprocessable", "messages days must be within 30 to 395", undefined, "retention_out_of_bounds"),
      }),
    );
    const { user, table } = await open();
    await user.click(within(table).getByRole("button", { name: "Shorten Messages" }));
    const dialog = await screen.findByRole("dialog");
    await user.selectOptions(within(dialog).getByLabelText("Unit"), "days");
    const amount = within(dialog).getByLabelText("Keep for");
    await user.clear(amount);
    await user.type(amount, "90");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    expect(await within(dialog).findByText(/must be within 30 to 395/)).toBeInTheDocument();
    expect(amount).toHaveValue(90);
    expect(within(dialog).getByLabelText("Unit")).toHaveValue("days");
    expect(within(dialog).getByRole("button", { name: "Save" })).toBeEnabled();
  });

  it.each([
    ["403", () => problem(403, "Forbidden", "admin role required"), /admin role required/],
    ["500", () => problem(500, "Internal error", "the store is down"), /the store is down/],
    [
      "network",
      () => {
        throw new TypeError("Failed to fetch");
      },
      /Could not reach the server/,
    ],
  ])("shows a %s failure inline and keeps the dialog open", async (_name, handler, expected) => {
    mockApi(routes("admin", { [`PUT ${RETENTION}`]: handler }));
    const { user, table } = await open();
    await user.click(within(table).getByRole("button", { name: "Shorten Messages" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("button", { name: "Save" }));
    expect(await within(dialog).findByText(expected)).toBeInTheDocument();
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Save" })).toBeEnabled();
  });

  it("disables Save while the request is in flight so a double click sends one PUT", async () => {
    let release: (response: Response) => void = () => undefined;
    const pending = new Promise<Response>((resolve) => {
      release = resolve;
    });
    const calls = mockApi(routes("admin", { [`PUT ${RETENTION}`]: () => pending }));
    const { user, table } = await open();
    await user.click(within(table).getByRole("button", { name: "Shorten Messages" }));
    const dialog = await screen.findByRole("dialog");
    const save = within(dialog).getByRole("button", { name: "Save" });
    await user.dblClick(save);
    await waitFor(() => expect(within(dialog).getByRole("button", { name: "Saving" })).toBeDisabled());
    expect(calls.filter((call) => call.method === "PUT")).toHaveLength(1);
    release(json({ windows: windows(395) }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(calls.filter((call) => call.method === "PUT")).toHaveLength(1);
  });

  it("uses a row header for each window name", async () => {
    mockApi(routes("admin"));
    const { table } = await open();
    expect(within(table).getByRole("rowheader", { name: "Messages" })).toBeInTheDocument();
    expect(within(table).getAllByRole("rowheader")).toHaveLength(8);
  });

  it("keeps focus in the dialog and returns it to the opener on close", async () => {
    mockApi(routes("admin"));
    const { user, table } = await open();
    const opener = within(table).getByRole("button", { name: "Shorten Messages" });
    await user.click(opener);
    const dialog = await screen.findByRole("dialog");
    for (let i = 0; i < 6; i += 1) {
      await user.tab();
      expect(dialog).toContainElement(document.activeElement as HTMLElement);
    }
    await user.keyboard("{Escape}");
    await waitFor(() => expect(opener).toHaveFocus());
  });
});
