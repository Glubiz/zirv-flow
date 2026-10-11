import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { JobsPage } from "./JobsPage";

const DEAD_JOB = {
  id: "7f3c0000-0000-4000-8000-00000000a91e",
  kind: "flow.step",
  state: "dead",
  attempts: 5,
  max_attempts: 5,
  last_error: "the upstream refused the request",
  trace_id: "trace-1",
  workspace_id: null,
  dead_at: "2026-10-11T07:00:00Z",
  cause: { kind: "schedule", id: "s1" },
};

const DEPTH = {
  kinds: [{ kind: "flow.step", ready: 2, running: 0, dead: 1, oldest_ready_seconds: 12 }],
};

function setup(retry: () => Response) {
  let retried = false;
  const calls = mockApi({
    "GET /api/v1/session": () => json(session(OWNER, "csrf-jobs")),
    "GET /api/v1/admin/jobs": () => json(DEPTH),
    "GET /api/v1/admin/jobs/dead": () => json({ jobs: retried ? [] : [DEAD_JOB], next_before: null }),
    [`POST /api/v1/admin/jobs/${DEAD_JOB.id}/retry`]: () => {
      retried = true;
      return retry();
    },
  });
  return calls;
}

describe("jobs admin", () => {
  it("retries a dead job, says so in plain words and refreshes the list", async () => {
    const calls = setup(() => json({ ...DEAD_JOB, state: "ready" }));
    const user = userEvent.setup();
    renderRoute("/jobs", "/jobs", <JobsPage />);

    await user.click(await screen.findByRole("button", { name: /Try again/ }));

    expect(await screen.findByText(/is back in the queue/)).toBeInTheDocument();
    expect(await screen.findByText("Nothing needs attention")).toBeInTheDocument();
    const retry = calls.find((call) => call.method === "POST");
    expect(retry?.headers.get("x-csrf-token")).toBe("csrf-jobs");
    expect(calls.filter((call) => call.path === "/api/v1/admin/jobs/dead")).toHaveLength(2);
  });

  it("explains a 409 conflict without hiding the job", async () => {
    setup(() => problem(409, "Conflict", "an equivalent job is already queued"));
    const user = userEvent.setup();
    renderRoute("/jobs", "/jobs", <JobsPage />);

    await user.click(await screen.findByRole("button", { name: /Try again/ }));

    expect(await screen.findByText(/because an equivalent job is already waiting/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Try again/ })).toBeInTheDocument();
  });

  it("explains a 404 when the job is no longer dead", async () => {
    setup(() => problem(404, "Not found", "no such dead job"));
    const user = userEvent.setup();
    renderRoute("/jobs", "/jobs", <JobsPage />);

    await user.click(await screen.findByRole("button", { name: /Try again/ }));

    expect(await screen.findByText(/is no longer waiting to be retried/)).toBeInTheDocument();
  });

  it("tells a user who is not the instance owner that the screen is owner only, without calling the admin API", async () => {
    const calls = mockApi({ "GET /api/v1/session": () => json(session(MEMBER)) });
    renderRoute("/jobs", "/jobs", <JobsPage />);

    expect(await screen.findByText("Only the instance owner can see jobs")).toBeInTheDocument();
    expect(calls.some((call) => call.path.startsWith("/api/v1/admin"))).toBe(false);
  });
});
