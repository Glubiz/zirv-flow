import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes } from "react-router";
import { describe, expect, it, vi } from "vitest";

import { json, mockApi, OWNER, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { RunDialog } from "./RunDialog";
import { SourcePage } from "./SourcePage";
import { StreamsPanel } from "./StreamsPanel";
import { aFreshness, aRunDetail, aSource, aStream, aStreamingStream, KAFKA_KIND } from "./testData";

const BASE = "/api/v1/workspaces/ws1/sources/src-1";

function panel(streams: ReturnType<typeof aStream>[]) {
  render(<StreamsPanel workspaceId="ws1" sourceId="src-1" streams={streams} canManage={false} onChanged={vi.fn()} />);
}

describe("the lag of a streaming stream", () => {
  it("shows the total next to the freshness and the partitions in its title", () => {
    panel([
      aStreamingStream({
        freshness: aFreshness({
          lag: {
            total: 1204,
            partitions: [
              { partition: 0, lag: 4 },
              { partition: 1, lag: 1200 },
            ],
          },
        }),
      }),
    ]);

    const lag = screen.getByText("Lag 1,204");
    expect(lag).toHaveAttribute("title", "Partition 0: 4\nPartition 1: 1,200");
    expect(screen.getByText("Streaming")).toBeInTheDocument();
  });

  it("says it is waiting before the first fetch, and shows nothing for a stream that is not read", () => {
    panel([
      aStreamingStream({ freshness: aFreshness({ lag: null }) }),
      aStreamingStream({ id: "str-off", name: "paused", enabled: false }),
    ]);

    expect(screen.getAllByText("Waiting for the first fetch")).toHaveLength(1);
    expect(screen.queryByText(/^Lag /)).not.toBeInTheDocument();
  });

  it("shows no lag for a pull stream", () => {
    panel([aStream({ enabled: true, freshness: aFreshness() })]);

    expect(screen.queryByText(/Lag|Waiting for the first fetch/)).not.toBeInTheDocument();
  });
});

describe("setting up a streaming stream", () => {
  function routes(stream = aStreamingStream({ freshness: aFreshness({ lag: { total: 7, partitions: [] } }) })) {
    return {
      "GET /api/v1/session": () => json(session(OWNER, "csrf-k")),
      "GET /api/v1/workspaces/ws1/members": () =>
        json([{ user_id: OWNER.id, email: "o@example.test", role: "admin", added_at: "2026-10-01T09:00:00Z" }]),
      "GET /api/v1/source-kinds": () => json([KAFKA_KIND]),
      [`GET ${BASE}`]: () => json(aSource({ kind: "kafka" })),
      [`GET ${BASE}/streams`]: () => json([stream]),
      [`GET ${BASE}/runs`]: () => json({ runs: [], next_cursor: null }),
      [`PUT ${BASE}/streams/orders`]: () => json(stream),
    };
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

  it("hides the cursor, the reconciliation and the sweep and says the key is the message key", async () => {
    mockApi(routes());
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Set up orders" }));
    const dialog = within(screen.getByRole("dialog", { name: "Set up orders" }));

    expect(dialog.getByText(/Primary key is the message key/)).toBeInTheDocument();
    expect(dialog.queryByLabelText("Cursor field")).not.toBeInTheDocument();
    expect(dialog.queryByLabelText("Reconciliation schedule")).not.toBeInTheDocument();
    expect(dialog.queryByLabelText("Largest share a run may remove (%)")).not.toBeInTheDocument();
    expect(dialog.queryByRole("group", { name: "Primary key" })).not.toBeInTheDocument();
    expect(dialog.getByText("Behind by")).toBeInTheDocument();
  });

  it("saves the message key as the primary key with no cursor and the default sweep", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Set up orders" }));
    await user.click(
      within(screen.getByRole("dialog", { name: "Set up orders" })).getByRole("button", { name: "Save stream" }),
    );

    await waitFor(() => expect(calls.filter((call) => call.method === "PUT")).toHaveLength(1));
    expect(calls.find((call) => call.method === "PUT")!.body).toMatchObject({
      sync_mode: "stream",
      cursor_field: null,
      primary_key: ["_key"],
      reconcile_schedule: null,
      sweep_limit: 0.5,
    });
  });
});

describe("the run of a streaming stream", () => {
  function runDialog(detail: ReturnType<typeof aRunDetail>) {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-k")),
      [`GET ${BASE}/runs/run-1`]: () => json(detail),
    });
    renderRoute(
      "/w/ws1/sources/src-1",
      "/w/:ws/sources/*",
      <RunDialog workspaceId="ws1" sourceId="src-1" runId="run-1" pollMs={60_000} onClose={vi.fn()} />,
    );
  }

  it("explains the long-lived run, names the mode and puts a refused message in words beside its place", async () => {
    runDialog(
      aRunDetail({
        sync_mode: "stream",
        state: "running",
        attempt: 1,
        rejects: [
          {
            id: "rej-k",
            received_at: "2026-10-03T09:00:05Z",
            reason: "kafka_value_not_json",
            reasons: [{ property: "partition 3 offset 1042", reason: "kafka_value_not_json" }],
            sample: { properties: {}, masked: [] } as never,
          },
        ],
      }),
    );

    expect(await screen.findByText("Streaming")).toBeInTheDocument();
    expect(screen.getByText(/goes on until it is cancelled or fails/)).toBeInTheDocument();
    expect(screen.getByText("partition 3 offset 1042: The message is not JSON.")).toBeInTheDocument();
    expect(screen.queryByText(/An earlier attempt was lost/)).not.toBeInTheDocument();
  });

  it("says an earlier attempt was lost only when the attempt count says so", async () => {
    runDialog(aRunDetail({ sync_mode: "stream", state: "running", attempt: 2, rejects: [] }));

    expect(await screen.findByText(/An earlier attempt was lost/)).toBeInTheDocument();
    expect(screen.getByText(/saved positions/)).toBeInTheDocument();
  });

  it("explains a consumer that failed on its topic", async () => {
    runDialog(aRunDetail({ sync_mode: "stream", state: "failed", error_code: "kafka_topic_missing", rejects: [] }));

    expect(await screen.findByText(/topic of this stream does not exist/)).toBeInTheDocument();
    expect(screen.getByText("kafka_topic_missing")).toBeInTheDocument();
  });
});
