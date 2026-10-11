import { act, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { FakeEventSource } from "../../test/fakeEventSource";
import { json, MEMBER, mockApi, OWNER, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { DebuggerPage } from "./DebuggerPage";

function item(seq: number, overrides: Record<string, unknown> = {}) {
  return {
    seq,
    at: "2026-10-01T09:00:00Z",
    source: "web",
    name: "order",
    outcome: "accepted",
    reasons: [],
    reason: null,
    properties: { plan: "pro", email: null },
    masked: ["email"],
    event_id: "e1",
    reject_id: null,
    trace_id: "0".repeat(32),
    ...overrides,
  };
}

function members(role: "admin" | "member", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function signedInAs(user: typeof OWNER, role: "admin" | "member") {
  mockApi({
    "GET /api/v1/session": () => json(session(user)),
    "GET /api/v1/workspaces/ws1/members": () => json(members(role, user.id)),
  });
}

function open() {
  return renderRoute("/w/ws1/events/debugger", "/w/:ws/events/debugger", <DebuggerPage />);
}

async function connected() {
  await screen.findByText("Connecting");
  act(() => {
    FakeEventSource.latest.open();
  });
  await screen.findByText("Live");
  return FakeEventSource.latest;
}

describe("event debugger", () => {
  beforeEach(() => {
    FakeEventSource.install();
  });

  it("opens the stream for the workspace and lists events newest first as they arrive", async () => {
    signedInAs(OWNER, "admin");
    open();
    const source = await connected();

    act(() => {
      source.emit("item", item(1, { name: "first_event" }), "1");
      source.emit("item", item(2, { name: "second_event", outcome: "flagged" }), "2");
    });

    expect(source.url).toBe("/api/v1/workspaces/ws1/debugger/stream");
    const table = await screen.findByRole("table", { name: "Live events" });
    const rows = within(table).getAllByRole("row").slice(1);
    expect(within(rows[0]!).getByText("second_event")).toBeInTheDocument();
    expect(within(rows[0]!).getByText("Flagged")).toBeInTheDocument();
    expect(within(rows[1]!).getByText("first_event")).toBeInTheDocument();
  });

  it("shows an empty state while nothing has arrived", async () => {
    signedInAs(OWNER, "admin");
    open();
    await connected();

    expect(screen.getByText("Waiting for events")).toBeInTheDocument();
  });

  it("discloses reasons and masked properties of a row and links its trace", async () => {
    signedInAs(OWNER, "admin");
    const user = userEvent.setup();
    open();
    const source = await connected();

    act(() => {
      source.emit(
        "item",
        item(1, {
          outcome: "rejected",
          reason: "validation_failed",
          reasons: [{ property: "total", reason: "wrong_type" }],
        }),
        "1",
      );
    });
    await user.click(await screen.findByText(/Show details/));

    expect(screen.getByText("Failed validation")).toBeVisible();
    expect(screen.getByText("A property has the wrong type")).toBeVisible();
    expect(screen.getByText("masked")).toBeVisible();
    expect(screen.getByText('"pro"')).toBeVisible();
    expect(screen.getByRole("link", { name: "Open the trace" })).toHaveAttribute("href", `/w/ws1/traces/${"0".repeat(32)}`);
  });

  it("holds new events while paused and adds them when resumed", async () => {
    signedInAs(OWNER, "admin");
    const user = userEvent.setup();
    open();
    const source = await connected();
    act(() => {
      source.emit("item", item(1, { name: "before_pause" }), "1");
    });
    await screen.findByText("before_pause");

    await user.click(screen.getByRole("button", { name: "Pause" }));
    act(() => {
      source.emit("item", item(2, { name: "while_paused" }), "2");
    });

    expect(screen.queryByText("while_paused")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Resume (1 held)" }));
    expect(await screen.findByText("while_paused")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Pause" })).toBeInTheDocument();
  });

  it("tells the viewer when the page could not keep up", async () => {
    signedInAs(OWNER, "admin");
    const user = userEvent.setup();
    open();
    const source = await connected();

    act(() => {
      source.emit("gap", { skipped: 40 });
    });

    expect(await screen.findByText(/about 40 events were skipped/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Dismiss" }));
    expect(screen.queryByText(/were skipped/)).not.toBeInTheDocument();
  });

  it("reports a lost connection and reconnects when asked", async () => {
    signedInAs(OWNER, "admin");
    const user = userEvent.setup();
    open();
    const first = await connected();

    act(() => {
      first.drop();
    });
    expect(await screen.findByText("Connection lost, reconnecting")).toBeInTheDocument();
    act(() => {
      first.fail();
    });
    expect(await screen.findByText("Disconnected")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Reconnect" }));

    expect(FakeEventSource.instances).toHaveLength(2);
    expect(first.readyState).toBe(2);
    expect(await screen.findByText("Connecting")).toBeInTheDocument();
  });

  it("reconnects with the filters and the reveal choice in the address", async () => {
    signedInAs(OWNER, "admin");
    const user = userEvent.setup();
    open();
    await connected();

    await user.type(screen.getByLabelText("Event name"), "order");
    await user.selectOptions(screen.getByLabelText("Outcome"), "rejected");
    await user.click(screen.getByRole("button", { name: "Apply filters" }));
    await user.click(await screen.findByLabelText(/Reveal PII/));

    const query = new URLSearchParams(FakeEventSource.latest.url.split("?")[1]);
    expect(query.get("name")).toBe("order");
    expect(query.get("outcome")).toBe("rejected");
    expect(query.get("reveal")).toBe("true");
  });

  it("offers no reveal to a member who is not an admin", async () => {
    signedInAs(MEMBER, "member");
    open();
    await connected();
    await screen.findByRole("button", { name: "Apply filters" });
    await act(async () => {
      await Promise.resolve();
    });

    expect(screen.queryByLabelText(/Reveal PII/)).not.toBeInTheDocument();
    expect(FakeEventSource.latest.url).not.toContain("reveal");
  });
});
