import { act, screen, waitFor } from "@testing-library/react";
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

  describe("when the trace id in the URL changes", () => {
    const OTHER = "fedcba9876543210fedcba9876543210";
    const records = (id: string) => json({ trace_id: id, truncated: false, records: [] });
    const routes = () =>
      mockApi({
        "GET /api/v1/session": () => json(session(OWNER)),
        [`GET /api/v1/workspaces/ws1/traces/${TRACE}`]: () => records(TRACE),
        [`GET /api/v1/workspaces/ws1/traces/${OTHER}`]: () => records(OTHER),
      });

    it("shows the new id and drops an error about the old one", async () => {
      routes();
      const user = userEvent.setup();
      const { router } = renderRoute(`/w/ws1/traces/${TRACE}`, "/w/:ws/traces/:traceId", <TracesPage />);
      const field = await screen.findByLabelText("Trace ID");
      await user.clear(field);
      await user.type(field, "GARBAGE");
      await user.click(screen.getByRole("button", { name: "Show trace" }));
      expect(await screen.findByRole("alert")).toHaveTextContent("32 lowercase");

      await act(async () => {
        await router.navigate(`/w/ws1/traces/${OTHER}`);
      });

      expect(screen.getByLabelText("Trace ID")).toHaveValue(OTHER);
      expect(screen.queryByRole("alert")).toBeNull();
    });

    it("empties the field and the result on the bare route", async () => {
      routes();
      const { router } = renderRoute(`/w/ws1/traces/${TRACE}`, "/w/:ws/traces/:traceId?", <TracesPage />);
      expect(await screen.findByLabelText("Trace ID")).toHaveValue(TRACE);
      await act(async () => {
        await router.navigate("/w/ws1/traces");
      });
      expect(screen.getByLabelText("Trace ID")).toHaveValue("");
      expect(screen.queryByText(/No records for this trace/)).toBeNull();
    });

    it("keeps focus in the form after a valid submit and keeps the error after a garbage one", async () => {
      routes();
      const user = userEvent.setup();
      renderRoute("/w/ws1/traces", "/w/:ws/traces/:traceId?", <TracesPage />);
      const field = await screen.findByLabelText("Trace ID");
      await user.type(field, "GARBAGE");
      await user.click(screen.getByRole("button", { name: "Show trace" }));
      expect(await screen.findByRole("alert")).toHaveTextContent("32 lowercase");

      await user.clear(field);
      await user.type(field, `${OTHER}{Enter}`);
      await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
      expect(screen.getByLabelText("Trace ID")).toBe(field);
      expect(field).toHaveFocus();
    });
  });
});
