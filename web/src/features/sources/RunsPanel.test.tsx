import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes } from "react-router";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { RunsPanel } from "./RunsPanel";
import { SourcePage } from "./SourcePage";
import { aRun, aRunDetail, aSource, aStream, FAKE_KIND } from "./testData";

const BASE = "/api/v1/workspaces/ws1/sources/src-1";

type Handlers = Record<string, () => Response | Promise<Response>>;

function members(role: "admin" | "member", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function mount() {
  return renderRoute(
    "/w/ws1/sources/src-1",
    "/w/:ws/sources/*",
    <Routes>
      <Route path=":sourceId" element={<SourcePage />} />
    </Routes>,
  );
}

function routes(role: "admin" | "member" = "admin", extra: Handlers = {}): Handlers {
  return {
    "GET /api/v1/session": () => json(session(role === "admin" ? OWNER : MEMBER, "csrf-runs")),
    "GET /api/v1/workspaces/ws1/members": () => json(members(role, role === "admin" ? OWNER.id : MEMBER.id)),
    "GET /api/v1/source-kinds": () => json([FAKE_KIND]),
    [`GET ${BASE}`]: () => json(aSource()),
    [`GET ${BASE}/streams`]: () =>
      json([
        aStream({
          enabled: true,
          mapping: { key_column: null, columns: [{ column: "email", target: { type: "ignore" }, transforms: [] }] },
        }),
      ]),
    [`GET ${BASE}/runs`]: () => json({ runs: [aRun()], next_cursor: null }),
    ...extra,
  };
}

async function runsTable() {
  return within(await screen.findByRole("table", { name: "Runs of this source" }));
}

describe("the runs of a source", () => {
  it("lists them with state, trigger, attempt, times and counters", async () => {
    mockApi(
      routes("admin", {
        [`GET ${BASE}/runs`]: () =>
          json({
            runs: [
              aRun(),
              aRun({
                id: "run-2",
                state: "running",
                trigger: "schedule",
                attempt: 2,
                finished_at: null,
                records_read: 500,
                records_applied: 500,
                records_rejected: 0,
                records_deleted: 0,
              }),
              aRun({ id: "run-3", state: "failed", error_code: "reject_threshold" }),
            ],
            next_cursor: null,
          }),
      }),
    );
    mount();

    const table = await runsTable();
    const rows = table.getAllByRole("row").slice(1);
    expect(rows).toHaveLength(3);
    expect(within(rows[0]!).getByText("Succeeded")).toBeInTheDocument();
    expect(within(rows[0]!).getByText("Manual")).toBeInTheDocument();
    expect(within(rows[1]!).getByText("Running")).toBeInTheDocument();
    expect(within(rows[1]!).getByText("Schedule")).toBeInTheDocument();
    expect(within(rows[1]!).getByText("500", { selector: "td:nth-child(7)" })).toBeInTheDocument();
    expect(within(rows[1]!).getByText("2", { selector: "td:nth-child(4)" })).toBeInTheDocument();
    expect(within(rows[2]!).getByText("Failed")).toBeInTheDocument();
  });

  it("says there are no runs yet", async () => {
    mockApi(routes("admin", { [`GET ${BASE}/runs`]: () => json({ runs: [], next_cursor: null }) }));
    mount();

    expect(await screen.findByText("No runs yet")).toBeInTheDocument();
  });

  it("shows a loading state and then an error with a retry", async () => {
    let attempts = 0;
    mockApi(
      routes("admin", {
        [`GET ${BASE}/runs`]: () => {
          attempts += 1;
          return attempts === 1
            ? problem(500, "Internal Server Error", "internal error")
            : json({ runs: [aRun()], next_cursor: null });
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    expect(await screen.findByText("Loading runs")).toBeInTheDocument();
    await user.click(await screen.findByRole("button", { name: "Try again" }));
    expect(await runsTable()).toBeTruthy();
  });

  it("offers older runs when there are more", async () => {
    const calls = mockApi(
      routes("admin", {
        [`GET ${BASE}/runs`]: () => json({ runs: [aRun()], next_cursor: "next-1" }),
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Show older runs" }));

    await waitFor(() => expect(calls.filter((call) => call.path === `${BASE}/runs`).length).toBeGreaterThan(1));
    expect(calls.some((call) => call.search.includes("cursor=next-1"))).toBe(true);
  });

  it("lets an admin run every stream, or one, and cancel an active run", async () => {
    const calls = mockApi(
      routes("admin", {
        [`GET ${BASE}/runs`]: () =>
          json({ runs: [aRun({ id: "run-9", state: "running", finished_at: null })], next_cursor: null }),
        [`POST ${BASE}/runs`]: () => json([aRun({ state: "queued" })], 202),
        [`POST ${BASE}/runs/run-9/cancel`]: () => json(aRun({ id: "run-9", state: "cancelled" })),
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Run now" }));
    expect(await screen.findByText("Started 1 run.")).toBeInTheDocument();
    await user.click(await screen.findByRole("button", { name: "Run stream_1 now" }));
    await user.click(await screen.findByRole("button", { name: "Cancel the stream_1 run" }));

    await waitFor(() => expect(calls.filter((call) => call.method === "POST")).toHaveLength(3));
    const posts = calls.filter((call) => call.method === "POST");
    expect(posts[0]!.body).toEqual({});
    expect(posts[1]!.body).toEqual({ stream: "stream_1" });
    expect(posts[2]!.path).toBe(`${BASE}/runs/run-9/cancel`);
  });

  it("shows why a start was refused", async () => {
    mockApi(
      routes("admin", {
        [`POST ${BASE}/runs`]: () => problem(409, "Conflict", "the stream already has a run that is queued or running"),
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Run now" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("the stream already has a run");
  });

  it("gives a member the history and the details but no way to start or cancel", async () => {
    mockApi(
      routes("member", {
        [`GET ${BASE}/runs`]: () => json({ runs: [aRun({ state: "running", finished_at: null })], next_cursor: null }),
      }),
    );
    mount();

    expect(await runsTable()).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Run now" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Run stream_1 now/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Cancel the/ })).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Details of the stream_1 run" })).toBeInTheDocument();
  });

  it("shows a run with its failure in plain words, the masked rejects and a link to the trace", async () => {
    mockApi(
      routes("member", {
        [`GET ${BASE}/runs/run-1`]: () =>
          json(aRunDetail({ state: "failed", error_code: "reject_threshold", attempt: 2 })),
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Details of the stream_1 run" }));

    const dialog = within(await screen.findByRole("dialog", { name: "Run details" }));
    expect(await dialog.findByText(/More than 5% of the records were refused/)).toBeInTheDocument();
    expect(dialog.getByText(/An earlier attempt was lost/)).toBeInTheDocument();
    const rejects = dialog.getByRole("table", { name: "Records this run refused" });
    expect(within(rejects).getByText("email: invalid_identifier")).toBeInTheDocument();
    expect(within(rejects).getByText("Ada")).toBeInTheDocument();
    expect(within(rejects).getByText("Hidden")).toBeInTheDocument();
    expect(dialog.getByRole("link", { name: "View trace" })).toHaveAttribute(
      "href",
      "/w/ws1/traces/0123456789abcdef0123456789abcdef",
    );
  });

  it("says a clean run refused nothing, and reports a failure to load it", async () => {
    let attempts = 0;
    mockApi(
      routes("member", {
        [`GET ${BASE}/runs/run-1`]: () => {
          attempts += 1;
          return attempts === 1
            ? problem(500, "Internal Server Error", "internal error")
            : json(aRunDetail({ rejects: [] }));
        },
      }),
    );
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Details of the stream_1 run" }));
    const dialog = within(await screen.findByRole("dialog", { name: "Run details" }));
    await user.click(await dialog.findByRole("button", { name: "Try again" }));

    expect(await dialog.findByText("No refused records")).toBeInTheDocument();
  });

  it("keeps polling while a run is active and stops once it ends", async () => {
    let reads = 0;
    mockApi(
      routes("admin", {
        [`GET ${BASE}/runs`]: () => {
          reads += 1;
          const state = reads < 3 ? "running" : "succeeded";
          return json({ runs: [aRun({ state, finished_at: state === "running" ? null : "2026-10-03T09:00:09Z" })], next_cursor: null });
        },
      }),
    );
    renderRoute(
      "/w/ws1/sources/src-1",
      "/w/:ws/sources/:sourceId",
      <RunsPanel workspaceId="ws1" sourceId="src-1" canManage version={0} pollMs={20} />,
    );

    expect(await screen.findByText("Running")).toBeInTheDocument();
    expect(await screen.findByText("Succeeded")).toBeInTheDocument();
    const settled = reads;
    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(reads).toBe(settled);
  });
});
