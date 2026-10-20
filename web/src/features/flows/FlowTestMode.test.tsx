import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { FLOW_ID, flowRoutes, WS } from "../../test/flowsApi";
import { mountEditor as mount, openSheetOf, saves } from "../../test/flowsEditor";
import { json, mockApi, problem, type Call } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

beforeEach(() => {
  stubReactFlow();
});

const TEST = `POST ${WS}/flows/${FLOW_ID}/test`;
const START = "2030-01-01T00:00:00Z";
const ENROLLMENT = "77777777-0000-4000-8000-000000000001";
const SNAPSHOT = {
  capped_events: [],
  consent: [],
  events: [],
  events_complete_after: null,
  identifier_kinds: [],
  lists: [],
  person_id: null,
  segments: [],
  traits: { city: "Copenhagen" },
};

function walk(overrides: Record<string, unknown> = {}) {
  return {
    enrollment_id: ENROLLMENT,
    external_effects: [],
    internal_effects: [],
    final_state: { clock: START, node: "n1", state_version: 1, status: "waiting", wake_at: "2030-01-02T00:00:00Z" },
    flow_id: FLOW_ID,
    halted: null,
    not_simulated: [],
    path: [{ at: START, node: "n1", outcome: "waiting", type: "wait.duration" }],
    person_after: { lists: [], traits: { city: "Copenhagen" } },
    person_id: null,
    script: [],
    snapshot: SNAPSHOT,
    source: "draft",
    start_at: START,
    version: null,
    waits: [],
    wakes: [],
    ...overrides,
  };
}

/** The same person a day later: the wait woke, the branch said no, and the webhook was captured. */
function afterADay() {
  return walk({
    final_state: { clock: "2030-01-02T00:00:00Z", node: "n5", state_version: 4, status: "completed" },
    path: [
      { at: START, node: "n1", outcome: "passed", type: "wait.duration", woke_by: "timer", woke_at: "2030-01-02T00:00:00Z" },
      {
        at: "2030-01-02T00:00:00Z",
        node: "n2",
        outcome: "branched",
        type: "branch",
        port: "else",
        branch: [
          {
            index: 0,
            outcome: "fail",
            observed: [{ pointer: "/conditions/0", outcome: "fail", observed: { value: "Copenhagen" } }],
          },
        ],
      },
      { at: "2030-01-02T00:00:00Z", node: "n4", outcome: "passed", type: "action.webhook" },
      { at: "2030-01-02T00:00:00Z", node: "n5", outcome: "completed", type: "exit" },
    ],
    script: [{ at: "2030-01-02T00:00:00Z", index: 0, kind: "advance_to", outcome: "advanced" }],
    external_effects: [
      {
        at: "2030-01-02T00:00:00Z",
        detail: {
          url: "https://crm.example.com/hooks",
          envelope: { flow_id: FLOW_ID, traits: { city: "Copenhagen" } },
        },
        effect_key: `${ENROLLMENT}:n4:webhook`,
        kind: "webhook",
        node: "n4",
        step_key: `${ENROLLMENT}:n4`,
      },
    ],
    internal_effects: [],
  });
}

async function openTest(user: ReturnType<typeof userEvent.setup>) {
  await screen.findByRole("group", { name: /^Wait: 1 day/ });
  await user.click(screen.getByRole("button", { name: "Test" }));
  return screen.findByRole("dialog", { name: "Set up a test run" });
}

async function startMadeUp(user: ReturnType<typeof userEvent.setup>) {
  const sheet = await openTest(user);
  await user.click(within(sheet).getByRole("radio", { name: /made-up person/ }));
  await user.click(within(sheet).getByRole("button", { name: "Start the test" }));
  return screen.findByRole("dialog", { name: "Test run" });
}

/** Waits for the editor's own check of the draft, so what comes after it is test mode's doing. Returns where test mode starts. */
async function settled(calls: Call[]) {
  await screen.findByRole("group", { name: /^Wait: 1 day/ });
  await waitFor(() => expect(calls.some((call) => call.path.endsWith("/validate"))).toBe(true));
  return calls.length;
}

function nonGet(calls: Call[], from = 0) {
  return calls.slice(from).filter((call) => call.method !== "GET").map((call) => `${call.method} ${call.path}`);
}

describe("test mode", () => {
  it("walks a person step by step, shows the path and the captured effects, and writes nothing", async () => {
    const calls = mockApi({
      ...flowRoutes(),
      [TEST]: async (request) => {
        const body = (await request.json()) as { script?: unknown[] };
        return json(body.script?.length ? afterADay() : walk());
      },
    });
    const user = userEvent.setup();
    mount();
    const from = await settled(calls);
    const sheet = await startMadeUp(user);

    // The first walk: the person is asleep at the wait, and the canvas numbers it.
    expect(within(sheet).getByText(/Nothing is sent or changed/)).toBeInTheDocument();
    expect(within(sheet).getByRole("list", { name: "Steps of the test path" })).toHaveTextContent("Waiting here");
    expect(await screen.findByRole("group", { name: /^Wait: 1 day.*on the path as step 1/ })).toBeInTheDocument();

    await user.click(within(sheet).getByRole("button", { name: "Advance the clock…" }));
    await user.click(within(sheet).getByRole("button", { name: "Advance" }));

    // Four steps now, with the branch result and the value it observed.
    const steps = await within(sheet).findByRole("list", { name: "Steps of the test path" });
    await waitFor(() => expect(within(steps).getAllByRole("listitem")).toHaveLength(4));
    const [wake, branch, hook, end] = within(steps).getAllByRole("listitem");
    expect(wake).toHaveTextContent("Woke on time");
    expect(branch).toHaveTextContent("Took No");
    expect(branch).toHaveTextContent('{"value":"Copenhagen"}');
    expect(hook).toHaveTextContent("Webhook");
    expect(end).toHaveTextContent("Completed the flow");

    // The effects are captured, not sent, and the envelope is shown.
    const effect = within(sheet).getByText("Not sent").closest(".fl-effect") as HTMLElement;
    expect(within(effect).getByText("POST https://crm.example.com/hooks")).toBeInTheDocument();
    expect(within(effect).getByLabelText("Webhook envelope")).toHaveTextContent(`"city": "Copenhagen"`);

    // The path is on the canvas: numbered, and what is off it is dimmed.
    expect(screen.getByRole("group", { name: /Webhook.*on the path as step 3/ })).toBeInTheDocument();
    const off = screen.getByRole("group", { name: /^Update a trait/ });
    expect(off.querySelector(".fl-node")).toHaveClass("fl-node-dim");
    expect(screen.getByRole("group", { name: /Webhook.*on the path/ }).querySelector(".fl-node")).not.toHaveClass("fl-node-dim");

    // The second walk re-posted the scenario with the snapshot the first one returned.
    const walks = calls.filter((call) => call.method === "POST" && call.path === `${WS}/flows/${FLOW_ID}/test`);
    expect(walks).toHaveLength(2);
    expect(walks[0]?.body).toEqual({ graph: expect.anything() as unknown, snapshot: {}, script: [] });
    expect(walks[1]?.body).toEqual({
      graph: expect.anything() as unknown,
      snapshot: SNAPSHOT,
      start_at: START,
      enrollment_id: ENROLLMENT,
      script: [{ advance_to: "2030-01-02T00:00:00.000Z" }],
    });

    // Test mode wrote nothing: the only write-method requests are the test walks.
    expect(nonGet(calls, from)).toEqual(Array(2).fill(`POST ${WS}/flows/${FLOW_ID}/test`));
  });

  it("walks the unsaved edit on the canvas and writes nothing of its own", async () => {
    const calls = mockApi({ ...flowRoutes(), [TEST]: () => json(walk()) });
    const user = userEvent.setup();
    mount();
    await settled(calls);

    // An edit the editor has not saved yet: the autosave waits 800 ms, and the walk below is far inside that.
    const step = await openSheetOf(user, /^Wait: 1 day/);
    const amount = within(step).getByLabelText("Amount");
    await user.clear(amount);
    await user.type(amount, "2");
    expect(saves(calls)).toHaveLength(0);

    // From the Test click to the end of the walk, only the walk itself may start.
    const from = calls.length;
    await user.click(screen.getByRole("button", { name: "Test" }));
    const sheet = await screen.findByRole("dialog", { name: "Set up a test run" });
    await user.click(within(sheet).getByRole("radio", { name: /made-up person/ }));
    await user.click(within(sheet).getByRole("button", { name: "Start the test" }));
    await screen.findByRole("dialog", { name: "Test run" });

    // The walk carried the edited draft, though nothing had been saved when it was asked.
    const walks = calls.filter((call) => call.method === "POST" && call.path === `${WS}/flows/${FLOW_ID}/test`);
    expect(walks).toHaveLength(1);
    const graph = (walks[0]?.body as { graph: { nodes: { id: string; seconds?: number }[] } }).graph;
    expect(graph.nodes.find((node) => node.id === "n1")?.seconds).toBe(172_800);
    expect(nonGet(calls, from)).toEqual([`POST ${WS}/flows/${FLOW_ID}/test`]);
  });

  it("says plainly that the walk cannot decide a condition, with the partial path", async () => {
    mockApi({
      ...flowRoutes(),
      [TEST]: () =>
        json(
          walk({
            halted: {
              reason: "unable_to_evaluate",
              message: "The branch reads an event the snapshot cut off.",
              node: "n2",
              condition: 0,
              cap: 100,
              capped_events: ["order"],
            },
            final_state: { clock: START, node: "n2", state_version: 2, status: "awaiting_conditions" },
            path: [
              { at: START, node: "n1", outcome: "passed", type: "wait.duration" },
              { at: START, node: "n2", outcome: "unable_to_evaluate", type: "branch" },
            ],
          }),
        ),
    });
    const user = userEvent.setup();
    mount();
    const sheet = await startMadeUp(user);

    const alert = await within(sheet).findByRole("alert");
    expect(alert).toHaveTextContent("Unable to evaluate");
    expect(alert).toHaveTextContent("The branch reads an event the snapshot cut off.");
    expect(alert).toHaveTextContent("order");
    expect(within(sheet).getByRole("list", { name: "Steps of the test path" })).toHaveTextContent("Unable to evaluate");
  });

  it("walks a real person found by search, for an admin", async () => {
    const calls = mockApi({
      ...flowRoutes(),
      [`POST ${WS}/people/search`]: () =>
        json({ next_after: null, people: [{ id: "88888888-0000-4000-8000-000000000001", updated_at: START, display: { kind: "email", masked: false, value: "dana@example.com" } }] }),
      [TEST]: () => json(walk({ person_id: "88888888-0000-4000-8000-000000000001" })),
    });
    const user = userEvent.setup();
    mount();
    const from = await settled(calls);
    const sheet = await openTest(user);

    await user.type(within(sheet).getByLabelText("Search people"), "dana@example.com");
    await user.click(within(sheet).getByRole("button", { name: "Search" }));
    await user.click(await within(sheet).findByRole("button", { name: "Use dana@example.com" }));
    await user.click(within(sheet).getByRole("button", { name: "Start the test" }));

    await screen.findByRole("dialog", { name: "Test run" });
    const walks = calls.filter((call) => call.path === `${WS}/flows/${FLOW_ID}/test`);
    expect(walks[0]?.body).toEqual({ graph: expect.anything() as unknown, person_id: "88888888-0000-4000-8000-000000000001", script: [] });
    expect(nonGet(calls, from)).toEqual([`POST ${WS}/people/search`, `POST ${WS}/flows/${FLOW_ID}/test`]);
  });

  it("does not offer a real person to someone who may not see people's values", async () => {
    mockApi(flowRoutes({ role: "member" }));
    const user = userEvent.setup();
    mount();
    const sheet = await openTest(user);

    expect(within(sheet).getByRole("radio", { name: "A real person" })).toBeDisabled();
    expect(within(sheet).getByRole("radio", { name: /made-up person/ })).toBeChecked();
    expect(within(sheet).getByText(/needs a workspace admin/)).toBeInTheDocument();
  });

  it("shows a refusal of the walk and keeps the earlier result", async () => {
    let asked = 0;
    mockApi({
      ...flowRoutes(),
      [TEST]: () => {
        asked += 1;
        return asked === 1 ? json(walk()) : problem(422, "Unprocessable", "the script is not valid");
      },
    });
    const user = userEvent.setup();
    mount();
    const sheet = await startMadeUp(user);
    await user.click(within(sheet).getByRole("button", { name: "Advance the clock…" }));
    await user.click(within(sheet).getByRole("button", { name: "Advance" }));

    expect(await within(sheet).findByRole("alert")).toHaveTextContent("the script is not valid");
    expect(within(sheet).getByRole("list", { name: "Steps of the test path" })).toBeInTheDocument();
  });

  it("checks the snapshot before asking, and sends nothing when it is not JSON", async () => {
    const calls = mockApi({ ...flowRoutes(), [TEST]: () => json(walk()) });
    const user = userEvent.setup();
    mount();
    const sheet = await openTest(user);
    await user.click(within(sheet).getByRole("radio", { name: /made-up person/ }));
    const box = within(sheet).getByLabelText("Snapshot");
    await user.clear(box);
    await user.type(box, "not json");
    await user.click(within(sheet).getByRole("button", { name: "Start the test" }));

    expect(await within(sheet).findByRole("alert")).toHaveTextContent("not valid JSON");
    expect(calls.filter((call) => call.path.endsWith("/test"))).toHaveLength(0);
  });

  it("is reached and left from the keyboard, and focus goes back to the Test button", async () => {
    mockApi({ ...flowRoutes(), [TEST]: () => json(walk()) });
    const user = userEvent.setup();
    mount();
    await screen.findByRole("group", { name: /^Wait: 1 day/ });
    const toggle = screen.getByRole("button", { name: "Test" });
    toggle.focus();
    await user.keyboard("{Enter}");

    const sheet = await screen.findByRole("dialog", { name: "Set up a test run" });
    expect(sheet).toHaveFocus();
    expect(screen.getByRole("button", { name: "Exit test" })).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByText("Test mode")).toBeInTheDocument();

    await user.keyboard("{Escape}");

    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Set up a test run" })).not.toBeInTheDocument());
    expect(screen.queryByText("Test mode")).not.toBeInTheDocument();
    await waitFor(() => expect(screen.getByRole("button", { name: "Test" })).toHaveFocus());
  });

  it("is not offered to someone who can only look", async () => {
    mockApi(flowRoutes({ role: "reader" }));
    mount();
    await screen.findByRole("group", { name: /^Wait: 1 day/ });

    expect(screen.queryByRole("button", { name: "Test" })).not.toBeInTheDocument();
  });
});
