import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { RejectsPage } from "./RejectsPage";

const REJECT = {
  id: "r1",
  received_at: "2026-10-01T09:00:00Z",
  source: "web",
  kind: "event",
  name: "order",
  reason: "validation_failed",
  reasons: [
    { property: "total", reason: "wrong_type" },
    { property: "card", reason: "unplanned_property" },
  ],
  sample: { properties: { total: "free", card: null }, masked: ["card"] },
  person_id: null,
  message_id: null,
  trace_id: "0".repeat(32),
  replayed_at: null,
};

const URI = "/api/v1/workspaces/ws1/ingest-rejects";

function open() {
  return renderRoute("/w/ws1/events/rejects", "/w/:ws/events/rejects", <RejectsPage />);
}

function mountWith(handler: (request: Request) => Response) {
  return mockApi({
    "GET /api/v1/session": () => json(session(OWNER)),
    [`GET ${URI}`]: handler,
  });
}

describe("rejects", () => {
  it("lists the reason and each failing property, and expands a masked sample", async () => {
    mountWith(() => json({ rejects: [REJECT], next_before: null }));
    const user = userEvent.setup();
    open();

    const table = await screen.findByRole("table", { name: "Rejected events" });
    expect(within(table).getByText("Failed validation")).toBeInTheDocument();
    expect(within(table).getByText("A property has the wrong type")).toBeInTheDocument();
    expect(within(table).getByText("A property is not in the schema")).toBeInTheDocument();
    expect(within(table).getByText("web")).toBeInTheDocument();

    await user.click(within(table).getByText(/Show sample/));
    expect(within(table).getByText('"free"')).toBeVisible();
    expect(within(table).getByText("masked")).toBeVisible();
    expect(within(table).queryByText("null")).not.toBeInTheDocument();
  });

  it("shows an empty state and a retry", async () => {
    mountWith(() => json({ rejects: [], next_before: null }));
    const first = open();
    expect(await screen.findByText("No rejects")).toBeInTheDocument();
    first.unmount();

    mountWith(() => problem(500, "Internal Server Error", "internal error"));
    open();
    expect(await screen.findByRole("button", { name: /try again/i })).toBeInTheDocument();
  });

  it("filters by event name and reason", async () => {
    const calls = mountWith(() => json({ rejects: [], next_before: null }));
    const user = userEvent.setup();
    open();

    await screen.findByText("No rejects");
    await user.type(screen.getByLabelText("Event name"), "order");
    await user.type(screen.getByLabelText("Reason code"), "validation_failed");
    await user.click(screen.getByRole("button", { name: "Filter" }));

    expect(await screen.findByText("No rejects match")).toBeInTheDocument();
    const last = calls.filter((call) => call.path === URI).at(-1);
    expect(new URLSearchParams(last?.search).get("name")).toBe("order");
    expect(new URLSearchParams(last?.search).get("reason")).toBe("validation_failed");
  });

  it("pages to older rejects with the cursor and back to the newest", async () => {
    const calls = mountWith((request) => {
      const before = new URL(request.url).searchParams.get("before");
      return before === null
        ? json({ rejects: [REJECT], next_before: "cursor-1" })
        : json({ rejects: [{ ...REJECT, id: "r2", name: "older_event" }], next_before: null });
    });
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole("button", { name: "Older rejects" }));

    expect(await screen.findByText("older_event")).toBeInTheDocument();
    expect(new URLSearchParams(calls.filter((call) => call.path === URI).at(-1)?.search).get("before")).toBe("cursor-1");
    expect(screen.queryByRole("button", { name: "Older rejects" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Back to the newest" }));
    expect(await screen.findByText("order")).toBeInTheDocument();
  });
});

describe("replay", () => {
  const REPLAY_URI = "/api/v1/workspaces/ws1/ingest-rejects/replay";

  function admin(user: typeof OWNER, role: "admin" | "member", replay: (request: Request) => Response) {
    return mockApi({
      "GET /api/v1/session": () => json(session(user)),
      "GET /api/v1/workspaces/ws1/members": () =>
        json([{ user_id: user.id, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }]),
      [`GET ${URI}`]: () =>
        json({ rejects: [REJECT, { ...REJECT, id: "r-done", replayed_at: "2026-10-02T09:00:00Z" }], next_before: null }),
      [`POST ${REPLAY_URI}`]: replay,
    });
  }

  const PREVIEW = {
    dry_run: true,
    selected: 2,
    rejects: [
      { reject_id: "r1", outcome: "accept", reason: null, reasons: [], redacted_type_changed: [] },
      {
        reject_id: "r2",
        outcome: "still_rejected",
        reason: "validation_failed",
        reasons: [{ property: "total", reason: "wrong_type" }],
        redacted_type_changed: ["total"],
      },
    ],
    totals: { accept: 1, flag: 0, still_rejected: 1, duplicate: 0, already_replayed: 0 },
    truncated: false,
    trace_id: null,
  };

  it("offers no replay to a member who is not an admin", async () => {
    admin(MEMBER, "member", () => json(PREVIEW));
    open();

    await screen.findByRole("table", { name: "Rejected events" });
    expect(screen.queryByRole("button", { name: /Replay/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("checkbox")).not.toBeInTheDocument();
  });

  it("selects rejects, previews what replaying would do and queues it on confirmation", async () => {
    const calls = admin(OWNER, "admin", () => json(PREVIEW));
    const user = userEvent.setup();
    open();

    const table = await screen.findByRole("table", { name: "Rejected events" });
    expect(within(table).getByText("Replayed")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Replay selected" })).toBeDisabled();
    await user.click(within(table).getByRole("checkbox"));
    await user.click(screen.getByRole("button", { name: "Replay selected (1)" }));

    const dialog = await screen.findByRole("dialog", { name: "Replay rejected events" });
    expect(await within(dialog).findByText("Will be stored")).toBeInTheDocument();
    expect(within(dialog).getByText("Still rejected")).toBeInTheDocument();
    expect(within(dialog).getByText(/Redaction changed the type of/)).toBeInTheDocument();
    const preview = calls.filter((call) => call.path === REPLAY_URI).at(-1)?.body as Record<string, unknown>;
    expect(preview).toMatchObject({ ids: ["r1"], dry_run: true });
    const triggers = within(dialog).getByLabelText(/Emit downstream triggers/);
    expect(triggers).not.toBeChecked();
    expect(within(dialog).getByText(/no_triggers/)).toBeInTheDocument();
  });

  it("sends the cause and the trigger choice only when confirmed, then shows the trace", async () => {
    const calls = admin(OWNER, "admin", () => json(PREVIEW));
    const user = userEvent.setup();
    open();
    await user.click(await screen.findByRole("button", { name: "Replay all open rejects" }));
    const dialog = await screen.findByRole("dialog", { name: "Replay rejected events" });
    await within(dialog).findByText("Will be stored");
    expect(calls.filter((call) => call.path === REPLAY_URI)).toHaveLength(1);
    await user.selectOptions(within(dialog).getByLabelText("Why are you replaying?"), "manual");
    await user.click(within(dialog).getByLabelText(/Emit downstream triggers/));

    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json([{ user_id: OWNER.id, email: "o@example.test", role: "admin", added_at: "2026-10-01T09:00:00Z" }]),
      [`GET ${URI}`]: () => json({ rejects: [REJECT], next_before: null }),
      [`POST ${REPLAY_URI}`]: () => json({ dry_run: false, selected: 2, rejects: [], totals: PREVIEW.totals, trace_id: "a".repeat(32) }, 202),
    });
    await user.click(within(dialog).getByRole("button", { name: "Replay 1 event" }));

    expect(await screen.findByText(/2 rejects are being replayed/)).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Follow the decisions on the trace" })).toHaveAttribute("href", `/w/ws1/traces/${"a".repeat(32)}`);
  });

  it("explains an error and keeps confirm off when nothing would be stored", async () => {
    admin(OWNER, "admin", () => problem(422, "Unprocessable Entity", "a replay covers at most 10000 rejects"));
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole("button", { name: "Replay all open rejects" }));

    const dialog = await screen.findByRole("dialog", { name: "Replay rejected events" });
    expect(await within(dialog).findByText(/at most 10000 rejects/)).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Nothing to replay" })).toBeDisabled();
  });

  it("replays by the filter that is applied", async () => {
    const calls = admin(OWNER, "admin", () => json(PREVIEW));
    const user = userEvent.setup();
    open();
    await screen.findByRole("table", { name: "Rejected events" });
    await user.type(screen.getByLabelText("Event name"), "order");
    await user.click(screen.getByRole("button", { name: "Filter" }));

    await user.click(await screen.findByRole("button", { name: "Replay all that match the filter" }));

    await screen.findByRole("dialog", { name: "Replay rejected events" });
    const body = calls.filter((call) => call.path === REPLAY_URI).at(-1)?.body as Record<string, unknown>;
    expect(body).toMatchObject({ filter: { name: "order" }, dry_run: true });
  });

  it("says when the table lists only the first rejects of a large replay", async () => {
    admin(OWNER, "admin", () => json({ ...PREVIEW, selected: 5000, truncated: true }));
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole("button", { name: "Replay all open rejects" }));

    const dialog = await screen.findByRole("dialog", { name: "Replay rejected events" });
    expect(await within(dialog).findByText(/lists the first 2 of 5000 rejects/)).toBeInTheDocument();
  });
});
