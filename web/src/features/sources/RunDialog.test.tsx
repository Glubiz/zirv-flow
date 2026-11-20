import { screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { json, mockApi, OWNER, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { RunDialog } from "./RunDialog";
import { aRunDetail } from "./testData";

const BASE = "/api/v1/workspaces/ws1/sources/src-1";

describe("polling a run", () => {
  it("ends on the finished state when every GET is slower than the poll interval", async () => {
    let gets = 0;
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-rd")),
      [`GET ${BASE}/runs/run-1`]: async () => {
        gets += 1;
        const snapshot =
          gets === 1
            ? aRunDetail({ state: "running", finished_at: null, rejects: [] })
            : aRunDetail({ state: "succeeded", rejects: [] });
        await new Promise((resolve) => setTimeout(resolve, 150));
        return json(snapshot);
      },
    });
    renderRoute(
      "/w/ws1/sources/src-1",
      "/w/:ws/sources/*",
      <RunDialog workspaceId="ws1" sourceId="src-1" runId="run-1" pollMs={50} onClose={vi.fn()} />,
    );

    expect(await screen.findByText("Running", undefined, { timeout: 3000 })).toBeInTheDocument();
    expect(await screen.findByText("Succeeded", undefined, { timeout: 3000 })).toBeInTheDocument();
  });
});
