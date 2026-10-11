import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { json, mockApi, OWNER, problem, session, type Call } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { ExplorerPage } from "./ExplorerPage";
import { buildFilter } from "./FilterBuilder";

const BASE = "/api/v1/workspaces/ws1";
const SCHEMAS = [
  {
    name: "order",
    mode: "warn",
    current_version: 1,
    has_draft: false,
    inferred: false,
    description: "",
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-01T09:00:00Z",
  },
];
const DETAIL = {
  schema: SCHEMAS[0],
  draft: null,
  versions: [
    {
      version: 1,
      published_at: "2026-10-01T09:00:00Z",
      properties: [
        { name: "total", type: "number", required: true },
        { name: "plan", type: "string" },
        { name: "email", type: "string", pii: true },
      ],
    },
  ],
};
const COUNT = {
  buckets: [
    { bucket_start: "2026-10-01T00:00:00Z", count: 4 },
    { bucket_start: "2026-10-02T00:00:00Z", count: 9 },
  ],
  total: 13,
  truncated: false,
};
const EVENT = {
  id: "ev1",
  occurred_at: "2026-10-02T09:00:00Z",
  name: "order",
  source: "web",
  person_id: "p1",
  validation_state: "valid",
  flags: ["late"],
  properties: { plan: "pro", email: null },
  masked: ["email"],
  trace_id: "0".repeat(32),
};

function routes(overrides: Record<string, () => Response> = {}) {
  return {
    "GET /api/v1/session": () => json(session(OWNER)),
    [`GET ${BASE}/event-schemas`]: () => json(SCHEMAS),
    [`GET ${BASE}/event-schemas/order`]: () => json(DETAIL),
    [`POST ${BASE}/events/explore/count`]: () => json(COUNT),
    [`POST ${BASE}/events/explore/breakdown`]: () =>
      json({ property: "plan", values: [{ value: "pro", count: 8 }, { value: "free", count: 3 }], other: 1, missing: 1 }),
    [`POST ${BASE}/events/explore/list`]: () => json({ events: [EVENT], next_after: null }),
    ...overrides,
  };
}

function open() {
  return renderRoute("/w/ws1/events/explorer", "/w/:ws/events/explorer", <ExplorerPage />);
}

function last(calls: Call[], path: string) {
  return calls.filter((call) => call.path === `${BASE}/events/explore/${path}`).at(-1);
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("event explorer", () => {
  it("runs the default query on load and draws the counts with a table fallback", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    open();

    expect(await screen.findByText("13 events in the range.")).toBeInTheDocument();
    expect(screen.getByRole("img", { name: /Events per day: 13 in total/ })).toBeInTheDocument();
    await user.click(screen.getByText("Show the numbers as a table"));
    const table = screen.getByRole("table", { name: "Events per day" });
    expect(within(table).getByText("9")).toBeVisible();
    const body = last(calls, "count")?.body as { bucket: string; from: string; to: string; filters: unknown[] };
    expect(body.bucket).toBe("day");
    expect(body.filters).toEqual([]);
    expect(new Date(body.to).getTime()).toBeGreaterThan(new Date(body.from).getTime());
    expect(await screen.findByRole("table", { name: "Matching events" })).toBeInTheDocument();
  });

  it("lists events with masked properties and links to the person and the trace", async () => {
    mockApi(routes());
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByText(/Show properties/));

    expect(screen.getByText("masked")).toBeVisible();
    expect(screen.getByText('"pro"')).toBeVisible();
    expect(screen.getByRole("link", { name: "Open the person" })).toHaveAttribute("href", "/w/ws1/people/p1");
    expect(screen.getByRole("link", { name: "Open the trace" })).toHaveAttribute("href", `/w/ws1/traces/${"0".repeat(32)}`);
  });

  it("types the filters by the schema of the chosen event and sends them", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    open();
    await screen.findByText("13 events in the range.");

    await user.selectOptions(screen.getByLabelText("Event"), "order");
    await user.click(screen.getByRole("button", { name: "Add a filter" }));
    await user.type(screen.getByLabelText("Property of filter 1"), "total");
    const operators = within(screen.getByLabelText("Operator of filter 1")).getAllByRole("option").map((option) => option.textContent);
    expect(operators).toContain("is greater than");
    expect(operators).not.toContain("starts with");
    await user.selectOptions(screen.getByLabelText("Operator of filter 1"), "gte");
    await user.type(screen.getByLabelText("Value of filter 1"), "20");
    await user.type(screen.getByLabelText("Break down by property"), "plan");
    await user.click(screen.getByRole("button", { name: "Run query" }));

    await screen.findByRole("table", { name: "Most common values of plan" });
    const body = last(calls, "count")?.body as { name: string; filters: unknown[] };
    expect(body.name).toBe("order");
    expect(body.filters).toEqual([{ key: "total", op: "gte", value: 20 }]);
    const table = screen.getByRole("table", { name: "Most common values of plan" });
    expect(within(table).getByText("pro")).toBeInTheDocument();
    expect(within(table).getByText("Other values")).toBeInTheDocument();
    expect(within(table).getByText("Without the property")).toBeInTheDocument();
  });

  it("refuses a filter without a value before asking the server", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    open();
    await screen.findByText("13 events in the range.");
    const before = calls.filter((call) => call.path.endsWith("/count")).length;

    await user.click(screen.getByRole("button", { name: "Add a filter" }));
    await user.type(screen.getByLabelText("Property of filter 1"), "plan");
    await user.click(screen.getByRole("button", { name: "Run query" }));

    expect(await screen.findByText('Enter a value for "plan".')).toBeInTheDocument();
    expect(calls.filter((call) => call.path.endsWith("/count"))).toHaveLength(before);
  });

  it("explains a query that ran too long and offers a retry", async () => {
    mockApi(
      routes({
        [`POST ${BASE}/events/explore/count`]: () =>
          problem(503, "Service Unavailable", "the query took too long; narrow the time range or the filters and try again"),
      }),
    );
    open();

    expect(await screen.findByText(/the query took too long/)).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: /try again/i }).length).toBeGreaterThan(0);
  });

  it("says so when nothing matches, and when more buckets matched than can be shown", async () => {
    mockApi(routes({ [`POST ${BASE}/events/explore/count`]: () => json({ buckets: [], total: 0, truncated: false }) }));
    const first = open();
    expect(await screen.findAllByText("No events match")).not.toHaveLength(0);
    first.unmount();

    mockApi(routes({ [`POST ${BASE}/events/explore/count`]: () => json({ ...COUNT, truncated: true }) }));
    open();
    expect(await screen.findByText(/The newest are left out/)).toBeInTheDocument();
  });

  it("pages to older events with the cursor", async () => {
    const calls = mockApi(
      routes({
        [`POST ${BASE}/events/explore/list`]: () => json({ events: [EVENT], next_after: "cursor-1" }),
      }),
    );
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole("button", { name: "Older events" }));

    await waitFor(() => {
      expect((last(calls, "list")?.body as { after?: string }).after).toBe("cursor-1");
    });
    expect(await screen.findByRole("button", { name: "Back to the newest" })).toBeInTheDocument();
  });

  it("exports the CSV of the same query as a download", async () => {
    const calls = mockApi(
      routes({
        [`POST ${BASE}/events/explore/export`]: () =>
          new Response("id,occurred_at\n", { status: 200, headers: { "content-type": "text/csv" } }),
      }),
    );
    const created = vi.fn(() => "blob:csv");
    const revoked = vi.fn();
    Object.assign(URL, { createObjectURL: created, revokeObjectURL: revoked });
    const clicked = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => undefined);
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole("button", { name: "Export CSV" }));

    await waitFor(() => {
      expect(clicked).toHaveBeenCalled();
    });
    expect(created).toHaveBeenCalled();
    expect(revoked).toHaveBeenCalledWith("blob:csv");
    expect(last(calls, "export")?.method).toBe("POST");
  });

  it("shows why an export failed", async () => {
    mockApi(
      routes({
        [`POST ${BASE}/events/explore/export`]: () => problem(503, "Service Unavailable", "the query took too long"),
      }),
    );
    const user = userEvent.setup();
    open();

    await user.click(await screen.findByRole("button", { name: "Export CSV" }));

    await act(async () => {
      await Promise.resolve();
    });
    expect(await screen.findByRole("alert")).toBeInTheDocument();
  });
});

describe("filter rows", () => {
  const row = (key: string, op: string, value: string) => ({ id: 1, key, op, value });

  it("types numbers, booleans, lists and presence by the property type", () => {
    expect(buildFilter(row("total", "eq", "5"), "number")).toEqual({ key: "total", op: "eq", value: 5 });
    expect(buildFilter(row("vip", "eq", "false"), "boolean")).toEqual({ key: "vip", op: "eq", value: false });
    expect(buildFilter(row("n", "in", "1, 2"), "integer")).toEqual({ key: "n", op: "in", value: [1, 2] });
    expect(buildFilter(row("plan", "in", "a, b"), "string")).toEqual({ key: "plan", op: "in", value: ["a", "b"] });
    expect(buildFilter(row("plan", "exists", "absent"), "string")).toEqual({ key: "plan", op: "exists", value: false });
    expect(buildFilter(row("plan", "exists", ""), "string")).toEqual({ key: "plan", op: "exists", value: true });
  });

  it("falls back to an operator the type takes, and refuses blanks and non-numbers", () => {
    expect(buildFilter(row("total", "prefix", "5"), "number")).toEqual({ key: "total", op: "eq", value: 5 });
    expect(buildFilter(row("", "eq", "x"), null)).toBe("Name the property for each filter.");
    expect(buildFilter(row("plan", "eq", " "), null)).toBe('Enter a value for "plan".');
    expect(buildFilter(row("total", "eq", "abc"), "number")).toBe('"total" takes a number.');
    expect(buildFilter(row("total", "in", "1, x"), "number")).toBe('"total" takes numbers.');
  });
});
