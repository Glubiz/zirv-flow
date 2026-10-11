import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, OWNER, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { TracesPage } from "./TracesPage";

const TRACE = "0123456789abcdef0123456789abcdef";

describe("trace inspector", () => {
  it("renders the records of a trace with action, outcome, cause, job and attempt", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      [`GET /api/v1/workspaces/ws1/traces/${TRACE}`]: () =>
        json({
          trace_id: TRACE,
          truncated: true,
          records: [
            {
              id: "r1",
              recorded_at: "2026-10-11T08:00:00Z",
              action: "job.attempt",
              outcome: "failed",
              cause: { kind: "api", id: "req-1" },
              job_id: "job-1",
              attempt: 2,
              reason: "upstream refused",
              subject_ids: { flow_id: "f-1" },
            },
          ],
        }),
    });
    renderRoute(`/w/ws1/traces/${TRACE}`, "/w/:ws/traces/:traceId", <TracesPage />);

    expect(await screen.findByText("job.attempt")).toBeInTheDocument();
    expect(screen.getByText("failed")).toBeInTheDocument();
    expect(screen.getByText("api req-1")).toBeInTheDocument();
    expect(screen.getByText("job-1")).toBeInTheDocument();
    expect(screen.getByText("Showing the first 1000 records")).toBeInTheDocument();
  });

  it("rejects a malformed trace id without calling the API", async () => {
    const calls = mockApi({ "GET /api/v1/session": () => json(session(OWNER)) });
    const user = userEvent.setup();
    renderRoute("/w/ws1/traces", "/w/:ws/traces", <TracesPage />);

    await user.type(await screen.findByLabelText("Trace ID"), "NOT-A-TRACE");
    await user.click(screen.getByRole("button", { name: "Show trace" }));

    expect(await screen.findByRole("alert")).toHaveTextContent("32 lowercase");
    expect(calls.filter((call) => call.path.includes("/traces/"))).toHaveLength(0);
  });
});
