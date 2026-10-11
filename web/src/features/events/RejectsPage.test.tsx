import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, OWNER, problem, session } from "../../test/mockApi";
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
