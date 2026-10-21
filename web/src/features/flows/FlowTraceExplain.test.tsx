import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { aiSettings, FLOW_ID, flowRoutes, versionBody, WS } from "../../test/flowsApi";
import { mountEditor as mount, type User } from "../../test/flowsEditor";
import { json, mockApi, problem, type Handler } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

beforeEach(() => {
  stubReactFlow();
});

const FLOW = `${WS}/flows/${FLOW_ID}`;
const TRACE = "0123456789abcdef0123456789abcdef";
const EXPLAIN = `POST ${WS}/traces/${TRACE}/ai/explain`;
const E1 = "99999999-0000-4000-8000-000000000001";
const PERSON = "88888888-0000-4000-8000-0000000000aa";

const CAUSE = { kind: "trait_change", id: "change-1" };

function decision(id: string, outcome: string, node?: string) {
  return {
    id,
    action: "flow.enrollment",
    outcome,
    cause: CAUSE,
    reason: null,
    recorded_at: "2026-10-20T09:00:00Z",
    subject_ids: { flow: FLOW_ID, version: 1, enrollment: E1, person: PERSON, ...(node ? { node } : {}) },
  };
}

const D1 = "aaaaaaaa-0000-4000-8000-000000000001";
const D2 = "aaaaaaaa-0000-4000-8000-000000000002";
const D3 = "aaaaaaaa-0000-4000-8000-000000000003";
const NOT_SHOWN = "aaaaaaaa-0000-4000-8000-0000000000ff";

const PATH = {
  enrollment: {
    id: E1,
    person_id: PERSON,
    state: "completed",
    node: "n5",
    version: 2,
    trace_id: TRACE,
    created_at: "2026-10-20T09:00:00Z",
    updated_at: "2026-10-20T09:00:00Z",
  },
  steps: [],
  decisions: [decision(D1, "enrolled"), decision(D2, "advanced", "n1"), decision(D3, "completed", "n5")],
  waits: [],
  truncated: false,
};

const EXPLAINED = {
  trace_id: TRACE,
  summary: "The person entered, waited a day and finished.",
  omitted_records: 0,
  records: [
    { id: D2, text: "They moved past the wait." },
    { id: NOT_SHOWN, text: "Something the list does not show." },
  ],
};

function routes(extra: Record<string, Handler> = {}, enabled = true): Record<string, Handler> {
  return {
    ...flowRoutes({ flow: { state: "published", current_version: 2 } }),
    [`GET ${WS}/ai/settings`]: () => json(aiSettings(enabled)),
    [`GET ${FLOW}/versions`]: () => json([versionBody(1), versionBody(2)]),
    [`GET ${FLOW}/versions/2/counts`]: () => json({ version: 2, nodes: [] }),
    [`GET ${FLOW}/enrollments`]: () => json({ enrollments: [PATH.enrollment], next_before: null }),
    [`GET ${FLOW}/enrollments/${E1}`]: () => json(PATH),
    [EXPLAIN]: () => json(EXPLAINED),
    ...extra,
  };
}

async function openEnrollment(user: User) {
  await screen.findByRole("group", { name: /^Wait: 1 day/ });
  await user.click(screen.getByRole("tab", { name: "Enrollments" }));
  const list = await screen.findByRole("dialog", { name: "Enrollments" });
  await user.click(await within(list).findByRole("button", { name: /Open the enrollment/ }));
  return screen.findByRole("dialog", { name: "One enrollment" });
}

describe("explaining a trace in the inspector", () => {
  it("explains the enrollment's trace and takes a cited decision to its row, highlighted", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mount();
    const sheet = await openEnrollment(user);

    await user.click(await within(sheet).findByRole("button", { name: "Explain this trace" }));

    expect(await within(sheet).findByText("The person entered, waited a day and finished.")).toBeInTheDocument();
    expect(calls.filter((call) => call.method === "POST" && call.path === `${WS}/traces/${TRACE}/ai/explain`)).toHaveLength(1);
    // The cited decision is marked in the tree; an uncited one is not.
    const cited = document.getElementById(`decision-${D2}`);
    expect(cited).toHaveClass("trace-record-cited");
    expect(document.getElementById(`decision-${D1}`)).not.toHaveClass("trace-record-cited");
    // A citation the list does not hold is text.
    expect(within(sheet).getByText("A decision that is not in the list below")).toBeInTheDocument();

    await user.click(within(sheet).getByRole("button", { name: /^Show the decision: flow\.enrollment advanced/ }));

    await waitFor(() => expect(cited).toHaveFocus());
    expect(cited).toHaveClass("trace-record-active");
    expect(cited).toHaveAttribute("aria-current", "true");
  });

  it("shows the reason when the explanation fails, and explains again on request", async () => {
    let asked = 0;
    mockApi(
      routes({
        [EXPLAIN]: () => {
          asked += 1;
          return asked === 1 ? problem(422, "Unprocessable Entity", "bad answers", "t-3", "ai_explain_failed") : json(EXPLAINED);
        },
      }),
    );
    const user = userEvent.setup();
    mount();
    const sheet = await openEnrollment(user);

    await user.click(await within(sheet).findByRole("button", { name: "Explain this trace" }));
    expect(await within(sheet).findByRole("alert")).toHaveTextContent("The assistant could not explain this");

    await user.click(within(sheet).getByRole("button", { name: "Explain again" }));
    expect(await within(sheet).findByText("The person entered, waited a day and finished.")).toBeInTheDocument();
  });

  it("is not offered when the assistant is off", async () => {
    const calls = mockApi(routes({}, false));
    const user = userEvent.setup();
    mount();
    const sheet = await openEnrollment(user);
    await waitFor(() => expect(calls.some((call) => call.path.endsWith("/ai/settings"))).toBe(true));

    await within(sheet).findByRole("list", { name: "Decisions on the trace of this enrollment" });
    expect(within(sheet).queryByRole("button", { name: "Explain this trace" })).not.toBeInTheDocument();
  });
});
