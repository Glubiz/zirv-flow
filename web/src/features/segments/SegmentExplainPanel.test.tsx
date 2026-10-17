import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem, type Call, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { actor, LISTS, PERSON_ID, refusal, SEGMENT_ID, segmentRow, VERSION_1, WS } from "../../test/segmentsApi";
import { SegmentExplainPanel } from "./SegmentExplainPanel";

type Node = {
  pointer: string;
  kind: string;
  outcome: "pass" | "fail" | "unknown";
  children: Node[];
  observed?: Record<string, unknown>;
  reason?: string;
};

function leaf(pointer: string, kind: string, outcome: Node["outcome"], extra: Partial<Node> = {}): Node {
  return { pointer, kind, outcome, children: [], ...extra };
}

function explainBody(tree: Node, extra: Record<string, unknown> = {}) {
  return {
    person_id: PERSON_ID,
    segment_id: SEGMENT_ID,
    version: 1,
    member: tree.outcome === "pass",
    entered_at: tree.outcome === "pass" ? "2026-10-05T09:00:00Z" : null,
    evaluated_at: "2026-10-10T09:00:00Z",
    outcome: tree.outcome,
    tree,
    ...extra,
  };
}

function routes(explain: Handler): Record<string, Handler> {
  return {
    ...actor("writer"),
    [`GET ${WS}/segments`]: () => json([segmentRow()]),
    [`GET ${WS}/lists`]: () => json(LISTS),
    [`GET ${WS}/segments/${SEGMENT_ID}/versions`]: () => json([VERSION_1]),
    [`GET ${WS}/people/${PERSON_ID}/explain`]: explain,
  };
}

async function choose(user: ReturnType<typeof userEvent.setup>) {
  await screen.findByRole("option", { name: "Big spenders" });
  await user.selectOptions(screen.getByLabelText("Segment"), SEGMENT_ID);
}

function mount() {
  return renderRoute(
    `/w/ws1/people/${PERSON_ID}`,
    "/w/:ws/people/:id",
    <SegmentExplainPanel workspaceId="ws1" personId={PERSON_ID} />,
  );
}

const PASSING = leaf("", "all", "pass", {
  children: [leaf("/all/0", "trait", "pass", { observed: { value: "Aarhus" } })],
});

describe("segment explain panel", () => {
  it("asks for a segment first, and says so when the workspace has none", async () => {
    mockApi({ ...routes(() => json(explainBody(PASSING))) });
    mount();
    expect(await screen.findByText("Choose a segment to see why this person is, or is not, in it.")).toBeInTheDocument();
  });

  it("offers a first segment when there are none", async () => {
    mockApi({ ...routes(() => json(explainBody(PASSING))), [`GET ${WS}/segments`]: () => json([]) });
    mount();
    expect(await screen.findByText("No segments yet")).toBeInTheDocument();
  });

  it("shows a loading state while it works out why", async () => {
    mockApi(routes(() => new Promise<Response>(() => undefined)));
    const user = userEvent.setup();
    mount();
    await choose(user);
    expect(await screen.findByText("Working out why")).toBeInTheDocument();
  });

  it("shows a passing tree with text and an icon, the stored membership and the condition in words", async () => {
    const calls = mockApi(routes(() => json(explainBody(PASSING))));
    const user = userEvent.setup();
    mount();
    await choose(user);

    expect(await screen.findByText("In the segment")).toBeInTheDocument();
    expect(screen.getByText(/Stored as a member since /)).toBeInTheDocument();
    expect(screen.queryByText(/A refresh is pending/)).not.toBeInTheDocument();
    const tree = screen.getByRole("tree", { name: "Why, condition by condition" });
    const items = within(tree).getAllByRole("treeitem");
    expect(items).toHaveLength(2);
    expect(within(items[1]!).getByText("Passes")).toBeInTheDocument();
    expect(within(items[1]!).getByText("✓")).toBeInTheDocument();
    expect(within(items[1]!).getByText("Field")).toBeInTheDocument();
    expect(within(items[1]!).getByText("Seen: Aarhus")).toBeInTheDocument();
    expect(explainQuery(calls)).toBe(`?segment=${SEGMENT_ID}`);
  });

  it("asks for the version the person picks", async () => {
    const calls = mockApi(routes(() => json(explainBody(PASSING))));
    const user = userEvent.setup();
    mount();
    await choose(user);
    await screen.findByText("In the segment");
    await user.selectOptions(screen.getByLabelText("Version"), "1");

    await screen.findByText("In the segment");
    expect(explainQuery(calls)).toBe(`?segment=${SEGMENT_ID}&version=1`);
  });

  it("shows a failing node as a failure, and notes a refresh is pending when the stored membership differs", async () => {
    const tree = leaf("", "all", "fail", {
      children: [leaf("/all/0", "trait", "fail", { observed: { missing: true } })],
    });
    mockApi(routes(() => json(explainBody(tree, { member: true, entered_at: "2026-10-05T09:00:00Z" }))));
    const user = userEvent.setup();
    mount();
    await choose(user);

    expect(await screen.findByText("Not in the segment")).toBeInTheDocument();
    expect(screen.getByText(/A refresh is pending/)).toBeInTheDocument();
    const items = screen.getAllByRole("treeitem");
    expect(within(items[1]!).getByText("Fails")).toBeInTheDocument();
    expect(within(items[1]!).getByText("✗")).toBeInTheDocument();
    expect(within(items[1]!).getByText("Seen: no value")).toBeInTheDocument();
  });

  it("explains an unknown node as unable to explain, and says why when too many events are capped", async () => {
    const tree = leaf("", "all", "unknown", {
      children: [
        leaf("/all/0", "trait", "unknown"),
        leaf("/all/1", "trait", "unknown", { reason: "capped" }),
      ],
    });
    mockApi(routes(() => json(explainBody(tree))));
    const user = userEvent.setup();
    mount();
    await choose(user);

    expect(await screen.findByText("Unable to tell whether this person is in the segment")).toBeInTheDocument();
    expect(screen.getByText("Unable to explain: too many events to load")).toBeInTheDocument();
    expect(screen.getAllByText("Unable to explain")).toHaveLength(2);
    expect(screen.getAllByText("?").length).toBeGreaterThan(0);
    expect(screen.queryByText(/A refresh is pending/)).not.toBeInTheDocument();
  });

  it("shows every kind of observed value as the API returns it", async () => {
    const tree = leaf("", "all", "pass", {
      children: [
        leaf("/all/0", "trait", "pass", { observed: { masked: true } }),
        leaf("/all/1", "event", "pass", { observed: { count: 3 } }),
        leaf("/all/2", "list", "pass", { observed: { present: true } }),
        leaf("/all/3", "segment", "fail", { observed: { present: false } }),
        leaf("/all/4", "consent", "pass", { observed: { status: "pending_confirmation" } }),
      ],
    });
    mockApi(routes(() => json(explainBody(tree))));
    const user = userEvent.setup();
    mount();
    await choose(user);

    await screen.findByText("In the segment");
    expect(screen.getByText(/Seen: 3 matching events/)).toBeInTheDocument();
    expect(screen.getByText("Seen: yes")).toBeInTheDocument();
    expect(screen.getByText("Seen: no")).toBeInTheDocument();
    expect(screen.getByText("Seen: consent is pending confirmation")).toBeInTheDocument();
    const masked = screen.getAllByRole("treeitem")[1]!;
    
    expect(within(masked).getByText("Hidden")).toBeInTheDocument();
  });

  it("notes that the profile of a merged person is shown", async () => {
    const mergedFrom = "77777777-0000-4000-8000-000000000001";
    mockApi(routes(() => json(explainBody(PASSING, { merged_from: mergedFrom }))));
    const user = userEvent.setup();
    mount();
    await choose(user);

    const note = await screen.findByText(/was merged into/);
    expect(note).toHaveTextContent("the profile of the merged person is explained");
    expect(within(note).getByRole("link", { name: "this person" })).toHaveAttribute("href", `/w/ws1/people/${PERSON_ID}`);
  });

  it("offers a retry when working it out takes too long", async () => {
    let asked = 0;
    mockApi(
      routes(() => {
        asked += 1;
        return asked === 1 ? refusal(504, "explain_timeout") : json(explainBody(PASSING));
      }),
    );
    const user = userEvent.setup();
    mount();
    await choose(user);

    expect(await screen.findByText("The explanation took too long")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByText("In the segment")).toBeInTheDocument();
  });

  it("says when the segment or person is not found", async () => {
    mockApi(routes(() => problem(404, "Not Found", "no such person")));
    const user = userEvent.setup();
    mount();
    await choose(user);

    expect(await screen.findByText("The segment, the version or the person was not found.")).toBeInTheDocument();
  });

  it("says when the version no longer validates", async () => {
    mockApi(routes(() => refusal(422, "segment_invalid")));
    const user = userEvent.setup();
    mount();
    await choose(user);

    expect(await screen.findByText("This version no longer validates")).toBeInTheDocument();
  });

  it("shows any other failure with a retry", async () => {
    mockApi(routes(() => problem(500, "Internal Server Error", "internal error")));
    const user = userEvent.setup();
    mount();
    await choose(user);

    expect(await screen.findByText("Internal Server Error")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Try again" })).toBeInTheDocument();
  });
});

function explainQuery(calls: Call[]): string | undefined {
  return calls.filter((call) => call.path === `${WS}/people/${PERSON_ID}/explain`).at(-1)?.search;
}
