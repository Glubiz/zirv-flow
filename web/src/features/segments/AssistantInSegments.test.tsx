import { isDeepStrictEqual } from "node:util";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes } from "react-router";
import { describe, expect, it } from "vitest";

import { json, mockApi, type Call, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import {
  actor,
  catalogRoutes,
  detail,
  previewBody,
  previewed,
  refusal,
  SEGMENT_ID,
  WS,
} from "../../test/segmentsApi";
import { NewSegmentPage } from "./NewSegmentPage";
import { SegmentPage } from "./SegmentPage";

const SEGMENT = `${WS}/segments/${SEGMENT_ID}`;
const SETTINGS = `GET ${WS}/ai/settings`;
const DRAFT = `POST ${WS}/ai/drafts`;
const ACCEPT = `POST ${WS}/ai/drafts/d1/accept`;
const EXPLAIN = `POST ${SEGMENT}/ai/explain`;

function aiSettings(enabled: boolean) {
  return {
    enabled,
    provider_id: "openai",
    model: "big",
    monthly_budget_micros: 1_000_000,
    pii_opt_in: false,
    transcript_days: null,
    provider_key_set: true,
    last_probe: null,
    updated_at: "2026-10-01T09:00:00Z",
  };
}

const DRAFTED = { v: 1, all: [{ trait: "city", op: "eq", value: "Odense" }] };
const DRAFTED_COUNT = 17;
const SAVED_COUNT = 41;

/** The headline count, which the page splits across two elements. */
function count(text: string) {
  return (_: string, element: Element | null) => element?.classList.contains("live-count") === true && element.textContent === text;
}

function drafted(overrides: Record<string, unknown> = {}) {
  return {
    id: "d1",
    kind: "segment",
    status: "valid",
    attempts: 1,
    content: DRAFTED,
    issues: [],
    changes: [
      {
        path: "/all",
        change: "changed",
        before: [{ trait: "city", op: "eq", value: "Aarhus" }],
        after: [{ trait: "city", op: "eq", value: "Odense" }],
      },
    ],
    dry_run: null,
    ...overrides,
  };
}

function base(kind: "writer" | "reader" = "writer", enabled = true): Record<string, Handler> {
  return {
    ...actor(kind),
    ...catalogRoutes(),
    [SETTINGS]: () => json(aiSettings(enabled)),
    [`POST ${WS}/segments/preview`]: () => json(previewBody()),
    [`GET ${SEGMENT}`]: () => json(detail()),
  };
}

function mountPage() {
  return renderRoute(
    `/w/ws1/segments/${SEGMENT_ID}`,
    "/w/:ws/*",
    <Routes>
      <Route path="segments/:id" element={<SegmentPage />} />
    </Routes>,
  );
}

function sent(calls: Call[], method: string, path: string): Call[] {
  return calls.filter((call) => call.method === method && call.path === path);
}

async function askToDraft(user: ReturnType<typeof userEvent.setup>, text = "Odense instead") {
  await user.type(await screen.findByLabelText("What do you want?"), text);
  await user.click(screen.getByRole("button", { name: "Draft" }));
}

describe("offering the assistant on a segment", () => {
  it("offers the draft panel and the explanation to an admin when the assistant is on", async () => {
    mockApi(base());
    mountPage();

    expect(await screen.findByRole("button", { name: "Explain this segment" })).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "Draft with the assistant" })).toBeInTheDocument();
  });

  it("offers nothing to a member, and never asks for the settings an admin alone may read", async () => {
    const calls = mockApi(base("reader"));
    mountPage();

    await screen.findByText("Big spenders");
    await screen.findByLabelText("Value");
    expect(screen.queryByRole("button", { name: "Explain this segment" })).not.toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Draft with the assistant" })).not.toBeInTheDocument();
    expect(sent(calls, "GET", `${WS}/ai/settings`)).toHaveLength(0);
  });

  it("offers nothing while the assistant is off", async () => {
    const calls = mockApi(base("writer", false));
    mountPage();

    await screen.findByLabelText("Value");
    await waitFor(() => expect(sent(calls, "GET", `${WS}/ai/settings`).length).toBeGreaterThan(0));
    expect(screen.queryByRole("button", { name: "Explain this segment" })).not.toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Draft with the assistant" })).not.toBeInTheDocument();
  });

  it("offers the draft panel on a new segment, with no segment to name", async () => {
    const calls = mockApi({ ...base(), [DRAFT]: () => json(drafted()) });
    const user = userEvent.setup();
    renderRoute(
      "/w/ws1/segments/new",
      "/w/:ws/*",
      <Routes>
        <Route path="segments/new" element={<NewSegmentPage />} />
      </Routes>,
    );

    await askToDraft(user, "Everyone in Odense");

    expect(await screen.findByRole("heading", { name: "Draft ready" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Explain this segment" })).not.toBeInTheDocument();
    expect(sent(calls, "POST", `${WS}/ai/drafts`)[0]?.body).toEqual({ kind: "segment", prompt: "Everyone in Odense" });
  });
});

describe("drafting a segment", () => {
  it("drafts for this segment, previews the draft with the live count and loads it as unsaved on accept", async () => {
    const calls = mockApi({
      ...base(),
      [`POST ${WS}/segments/preview`]: async (request) => {
        const { definition } = (await request.clone().json()) as { definition: unknown };
        return json(previewBody(isDeepStrictEqual(definition, DRAFTED) ? DRAFTED_COUNT : SAVED_COUNT));
      },
      [DRAFT]: () => json(drafted()),
      [ACCEPT]: () => json({ id: "d1", kind: "segment", segment_id: SEGMENT_ID, source_id: null, stream: null, content: DRAFTED }),
    });
    const user = userEvent.setup();
    mountPage();
    await screen.findByLabelText("Value");

    await askToDraft(user);

    expect(await screen.findByRole("heading", { name: "Draft ready" })).toBeInTheDocument();
    expect(sent(calls, "POST", `${WS}/ai/drafts`)[0]?.body).toEqual({
      kind: "segment",
      prompt: "Odense instead",
      segment_id: SEGMENT_ID,
    });
    const changes = screen.getByRole("table", { name: "What the draft changes" });
    expect(within(changes).getByText("Changed")).toBeInTheDocument();
    // The draft is counted with the builder's own preview request.
    expect(screen.getByRole("heading", { name: "Preview" })).toBeInTheDocument();
    await waitFor(() => expect(previewed(calls)).toContainEqual(DRAFTED));
    // The count shown beside the draft is the draft's own, not the saved definition's.
    const draftPreview = screen.getByRole("heading", { name: "Preview" }).nextElementSibling as HTMLElement;
    expect(await within(draftPreview).findByText(count(`${DRAFTED_COUNT} people match`))).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Accept" }));

    expect(await screen.findByText("Loaded into the editor. Save to apply it.")).toBeInTheDocument();
    expect(screen.getByLabelText("Value")).toHaveValue("Odense");
    expect(screen.getByText("Unsaved changes")).toBeInTheDocument();
    expect(screen.getByText("The assistant's draft is loaded in the builder. It is not saved yet.")).toBeInTheDocument();
    // Accepting saved nothing: saving is the person's own action.
    expect(sent(calls, "PATCH", SEGMENT)).toHaveLength(0);
    expect(screen.getByRole("button", { name: "Save" })).toBeEnabled();
  });

  it("asks in a dialog before replacing conditions that were edited, and replaces nothing if refused", async () => {
    const calls = mockApi({
      ...base(),
      [DRAFT]: () => json(drafted()),
      [ACCEPT]: () => json({ id: "d1", kind: "segment", segment_id: SEGMENT_ID, source_id: null, stream: null, content: DRAFTED }),
    });
    const user = userEvent.setup();
    mountPage();
    const value = await screen.findByLabelText("Value");
    await user.clear(value);
    await user.type(value, "Roskilde");
    await askToDraft(user);
    await user.click(await screen.findByRole("button", { name: "Accept" }));

    const dialog = await screen.findByRole("dialog", { name: "Replace your unsaved changes?" });
    await user.click(within(dialog).getByRole("button", { name: "Keep my changes" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(sent(calls, "POST", `${WS}/ai/drafts/d1/accept`)).toHaveLength(0);
    expect(screen.getByLabelText("Value")).toHaveValue("Roskilde");
    expect(screen.getByRole("button", { name: "Accept" })).toBeEnabled();

    await user.click(screen.getByRole("button", { name: "Accept" }));
    const again = await screen.findByRole("dialog", { name: "Replace your unsaved changes?" });
    await user.click(within(again).getByRole("button", { name: "Replace with the draft" }));

    expect(await screen.findByText("Loaded into the editor. Save to apply it.")).toBeInTheDocument();
    expect(sent(calls, "POST", `${WS}/ai/drafts/d1/accept`)).toHaveLength(1);
    expect(screen.getByLabelText("Value")).toHaveValue("Odense");
  });

  it("does not ask when the conditions are as saved", async () => {
    mockApi({
      ...base(),
      [DRAFT]: () => json(drafted()),
      [ACCEPT]: () => json({ id: "d1", kind: "segment", segment_id: SEGMENT_ID, source_id: null, stream: null, content: DRAFTED }),
    });
    const user = userEvent.setup();
    mountPage();
    await screen.findByLabelText("Value");
    await askToDraft(user);

    await user.click(await screen.findByRole("button", { name: "Accept" }));

    expect(await screen.findByText("Loaded into the editor. Save to apply it.")).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("says why an accepted draft no longer validates, in the words of the builder", async () => {
    mockApi({
      ...base(),
      [DRAFT]: () => json(drafted()),
      [ACCEPT]: () => refusal(409, "ai_draft_invalid", [{ pointer: "/all/0/trait", code: "unknown_trait" }]),
    });
    const user = userEvent.setup();
    mountPage();
    await screen.findByLabelText("Value");
    await askToDraft(user);

    await user.click(await screen.findByRole("button", { name: "Accept" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("This draft cannot be accepted");
    expect(alert).toHaveTextContent("The definition uses a field that does not exist in this workspace. (/all/0/trait)");
    expect(screen.getByLabelText("Value")).toHaveValue("Aarhus");
  });

  it("shows the problems of a draft that never validated in the words of a segment", async () => {
    mockApi({
      ...base(),
      [DRAFT]: () =>
        json(drafted({ status: "invalid", attempts: 3, content: null, changes: [], issues: [{ code: "unknown_trait", at: "/all/0/trait" }] })),
    });
    const user = userEvent.setup();
    mountPage();
    await screen.findByLabelText("Value");

    await askToDraft(user);

    const problems = await screen.findByRole("list", { name: "Problems with the last attempt" });
    expect(problems).toHaveTextContent("The definition uses a field that does not exist in this workspace. (/all/0/trait)");
    expect(screen.queryByRole("button", { name: "Accept" })).not.toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Preview" })).not.toBeInTheDocument();
  });
});

describe("explaining a segment", () => {
  const EXPLAINED = {
    version: 1,
    summary: "People who live in Aarhus.",
    nodes: [
      { pointer: "", text: "Everything below must hold." },
      { pointer: "/all/0", text: "<b>Aarhus</b> is where they live." },
    ],
  };

  it("shows a loading state, then the summary and each condition in words with the assistant's sentence", async () => {
    let answer: (response: Response) => void = () => undefined;
    const calls = mockApi({
      ...base(),
      [EXPLAIN]: () =>
        new Promise<Response>((resolve) => {
          answer = resolve;
        }),
    });
    const user = userEvent.setup();
    mountPage();

    await user.click(await screen.findByRole("button", { name: "Explain this segment" }));

    expect(await screen.findByText(/Asking the assistant to explain this segment/)).toBeInTheDocument();
    await waitFor(() => expect(sent(calls, "POST", `${SEGMENT}/ai/explain`)).toHaveLength(1));
    answer(json(EXPLAINED));
    expect(await screen.findByText("People who live in Aarhus.")).toBeInTheDocument();
    const list = screen.getByRole("list", { name: "Condition by condition" });
    const items = within(list).getAllByRole("listitem");
    expect(items).toHaveLength(2);
    expect(items[0]).toHaveTextContent("All of these are true");
    expect(items[0]).toHaveTextContent("Everything below must hold.");
    expect(items[1]).toHaveTextContent("city");
    expect(items[1]).toHaveTextContent("Aarhus");
    // The sentence is text: markup in it is shown as it was written, never rendered.
    expect(within(items[1] as HTMLElement).getByText("<b>Aarhus</b> is where they live.")).toBeInTheDocument();
    expect(list.querySelector("b")).toBeNull();
  });

  it("names the failure in words and asks again on retry", async () => {
    let attempts = 0;
    mockApi({
      ...base(),
      [EXPLAIN]: () => {
        attempts += 1;
        return attempts === 1 ? refusal(422, "ai_explain_failed", [{ pointer: "/all/0", code: "leaf_not_explained" }]) : json(EXPLAINED);
      },
    });
    const user = userEvent.setup();
    mountPage();
    await user.click(await screen.findByRole("button", { name: "Explain this segment" }));

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("The assistant could not explain this");
    await user.click(within(alert).getByRole("button", { name: "Try again" }));

    expect(await screen.findByText("People who live in Aarhus.")).toBeInTheDocument();
    expect(attempts).toBe(2);
  });

  it("says plainly when the budget is spent or the assistant was turned off meanwhile", async () => {
    const cases: [number, string, string][] = [
      [402, "ai_budget_exceeded", "The monthly AI budget is spent"],
      [409, "ai_disabled", "The AI assistant is off"],
    ];
    for (const [status, code, shown] of cases) {
      mockApi({ ...base(), [EXPLAIN]: () => refusal(status, code) });
      const user = userEvent.setup();
      const view = mountPage();
      await user.click(await screen.findByRole("button", { name: "Explain this segment" }));

      expect(await screen.findByRole("alert")).toHaveTextContent(shown);
      view.unmount();
    }
  });

  it("closes, and asks nothing until it is opened again", async () => {
    const calls = mockApi({ ...base(), [EXPLAIN]: () => json(EXPLAINED) });
    const user = userEvent.setup();
    mountPage();
    await user.click(await screen.findByRole("button", { name: "Explain this segment" }));
    await screen.findByText("People who live in Aarhus.");

    await user.click(screen.getByRole("button", { name: "Hide the explanation" }));

    expect(screen.queryByText("People who live in Aarhus.")).not.toBeInTheDocument();
    expect(sent(calls, "POST", `${SEGMENT}/ai/explain`)).toHaveLength(1);
    expect(screen.getByRole("button", { name: "Explain this segment" })).toBeInTheDocument();
  });
});
