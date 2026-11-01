import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { json, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { SessionProvider, useSession } from "../session";
import { AssistantPanel } from "./AssistantPanel";

const SETTINGS = "GET /api/v1/workspaces/ws1/ai/settings";
const DRAFT = "POST /api/v1/workspaces/ws1/ai/drafts";
const ACCEPT = "POST /api/v1/workspaces/ws1/ai/drafts/d1/accept";

function settings(enabled: boolean, piiOptIn = false) {
  return {
    enabled,
    provider_id: "openai",
    model: "big",
    monthly_budget_micros: 1_000_000,
    pii_opt_in: piiOptIn,
    transcript_days: null,
    provider_key_set: true,
    last_probe: null,
    updated_at: "2026-10-01T09:00:00Z",
  };
}

const CONTENT = { columns: [{ column: "email", target: { type: "identifier", kind: "email" } }] };

const VALID = {
  id: "d1",
  kind: "mapping",
  status: "valid",
  attempts: 2,
  content: CONTENT,
  issues: [],
  changes: [
    { path: "/columns/email", change: "added", before: null, after: CONTENT.columns[0] },
    { path: "/columns/name/target/key", change: "changed", before: "plan", after: "tier" },
  ],
  dry_run: {
    status: "ok",
    preview: {
      read: 1,
      truncated: false,
      records: [
        {
          key: "r1",
          deleted: false,
          result: "mapped",
          identifiers: [{ kind: "email", value: "ada@example.com" }],
          traits: { display_name: "Ada" },
          lists: [],
          event: null,
          errors: [],
        },
      ],
    },
  },
};

const INVALID = {
  id: "d2",
  kind: "mapping",
  status: "invalid",
  attempts: 3,
  content: null,
  issues: [{ code: "unknown_column", at: "ghost" }],
  changes: [],
  dry_run: null,
};

const BASE = { "GET /api/v1/session": () => json(session(OWNER, "csrf-a")) };

function Harness({
  onAccept,
  sample,
  confirmAccept,
}: {
  onAccept: (content: unknown) => void;
  sample?: unknown[] | null;
  confirmAccept?: () => boolean;
}) {
  const { state } = useSession();
  if (state.status !== "signed-in") return <p>Waiting for the session</p>;
  return (
    <AssistantPanel workspaceId="ws1" kind="mapping" sourceId="src-1" stream="contacts" sample={sample} onAccept={onAccept} confirmAccept={confirmAccept} />
  );
}

function mount(
  onAccept: (content: unknown) => void = () => undefined,
  sample?: unknown[] | null,
  confirmAccept?: () => boolean,
) {
  return render(
    <SessionProvider>
      <Harness onAccept={onAccept} sample={sample} confirmAccept={confirmAccept} />
    </SessionProvider>,
  );
}

async function ask(user: ReturnType<typeof userEvent.setup>, text = "Map the email") {
  await user.type(await screen.findByLabelText("Ask for a change"), text);
}

describe("the assistant panel", () => {
  it("is an empty state with a link to the settings while the assistant is off", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(false)) });
    mount();

    expect(await screen.findByText("The assistant is off")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Draft" })).not.toBeInTheDocument();
  });

  it("says what it sends, by the workspace's opt-in", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true, false)) });
    const first = mount();
    expect(await screen.findByText("Sends field names and types only.")).toBeInTheDocument();
    first.unmount();

    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true, true)) });
    mount();
    expect(await screen.findByText(/Also sends up to 5 sample records/)).toBeInTheDocument();
  });

  it("shows an error with a retry when the settings cannot be read", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => problem(500, "Internal Server Error", "internal error") });
    mount();

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent("Internal Server Error");
    expect(within(alert).getByRole("button", { name: "Try again" })).toBeInTheDocument();
  });

  it("shows a loading state while it drafts, with Cancel that stops it", async () => {
    mockApi({
      ...BASE,
      [SETTINGS]: () => json(settings(true)),
      [DRAFT]: (request) =>
        new Promise<Response>((_resolve, reject) => {
          request.signal.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")));
        }),
    });
    const user = userEvent.setup();
    mount();
    await ask(user);

    await user.click(screen.getByRole("button", { name: "Draft" }));

    expect(await screen.findByText("Drafting, this can take a minute")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(await screen.findByText("Cancelled. Nothing was kept.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Draft" })).toBeEnabled();
  });

  it("will not draft an empty prompt, and Ctrl+Enter in the prompt submits", async () => {
    const calls = mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [DRAFT]: () => json(VALID) });
    const user = userEvent.setup();
    mount();
    const area = await screen.findByLabelText("Ask for a change");
    expect(screen.getByRole("button", { name: "Draft" })).toBeDisabled();

    await user.type(area, "Map the email{Control>}{Enter}{/Control}");

    expect(await screen.findByRole("heading", { name: /^Draft ready/ })).toBeInTheDocument();
    const sent = calls.find((call) => call.path.endsWith("/ai/drafts"));
    expect(sent?.body).toMatchObject({ kind: "mapping", source_id: "src-1", stream: "contacts", prompt: "Map the email" });
  });

  it("shows the problems of an invalid draft and offers no way to accept it", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [DRAFT]: () => json(INVALID) });
    const user = userEvent.setup();
    mount();
    await ask(user);

    await user.click(screen.getByRole("button", { name: "Draft" }));

    const heading = await screen.findByRole("heading", { name: "No valid draft after 3 attempts" });
    expect(heading).toHaveFocus();
    const problems = screen.getByRole("list", { name: "Problems with the last attempt" });
    expect(problems).toHaveTextContent("The mapping uses a field that the stream does not have. (ghost)");
    expect(screen.queryByRole("button", { name: "Put into the draft" })).not.toBeInTheDocument();
    expect(screen.getByRole("status", { name: "" })).toHaveTextContent("No valid draft");
  });

  it("shows a valid draft with its changes and dry run, moves focus to it and announces it", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [DRAFT]: () => json(VALID) });
    const user = userEvent.setup();
    mount();
    await ask(user);

    await user.click(screen.getByRole("button", { name: "Draft" }));

    expect(await screen.findByRole("heading", { name: /^Draft ready/ })).toHaveFocus();
    const changes = screen.getByRole("table", { name: "What the draft changes" });
    expect(within(changes).getByText("columns › email")).toBeInTheDocument();
    expect(within(changes).getByText("Added")).toBeInTheDocument();
    expect(within(changes).getByText("Changed")).toBeInTheDocument();
    const dryRun = screen.getByRole("table", { name: "What the draft makes of the first records" });
    expect(within(dryRun).getByText("mapped")).toBeInTheDocument();
    // The dry run names the kinds and the keys, never the values of a person.
    expect(dryRun).toHaveTextContent("email");
    expect(dryRun).not.toHaveTextContent("ada@example.com");
    expect(screen.getAllByRole("status").some((node) => node.textContent === "Draft ready")).toBe(true);
  });

  it("accepts a valid draft, hands its content to the editor and says to save", async () => {
    const calls = mockApi({
      ...BASE,
      [SETTINGS]: () => json(settings(true)),
      [DRAFT]: () => json(VALID),
      [ACCEPT]: () => json({ id: "d1", kind: "mapping", source_id: "src-1", stream: "contacts", content: CONTENT }),
    });
    const onAccept = vi.fn();
    const user = userEvent.setup();
    mount(onAccept);
    await ask(user);
    await user.click(screen.getByRole("button", { name: "Draft" }));

    await user.click(await screen.findByRole("button", { name: "Put into the draft" }));

    expect(await screen.findByText("Loaded into the editor. Save to apply it.")).toBeInTheDocument();
    expect(onAccept).toHaveBeenCalledWith(CONTENT, expect.objectContaining({ id: "d1" }));
    expect(calls.some((call) => call.method === "PUT")).toBe(false);
    expect(screen.queryByRole("button", { name: "Put into the draft" })).not.toBeInTheDocument();
  });

  it("sends no accept request and keeps Put into the draft available when the host refuses the replacement", async () => {
    const calls = mockApi({
      ...BASE,
      [SETTINGS]: () => json(settings(true)),
      [DRAFT]: () => json(VALID),
      [ACCEPT]: () => json({ id: "d1", kind: "mapping", source_id: "src-1", stream: "contacts", content: CONTENT }),
    });
    const onAccept = vi.fn();
    const confirmAccept = vi.fn(() => false);
    const user = userEvent.setup();
    mount(onAccept, undefined, confirmAccept);
    await ask(user);
    await user.click(screen.getByRole("button", { name: "Draft" }));

    await user.click(await screen.findByRole("button", { name: "Put into the draft" }));

    expect(confirmAccept).toHaveBeenCalledTimes(1);
    expect(calls.some((call) => call.path.endsWith("/accept"))).toBe(false);
    expect(onAccept).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Put into the draft" })).toBeEnabled();
    expect(screen.queryByText("Loaded into the editor. Save to apply it.")).not.toBeInTheDocument();
  });

  it("discards a draft without accepting it", async () => {
    const onAccept = vi.fn();
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [DRAFT]: () => json(VALID) });
    const user = userEvent.setup();
    mount(onAccept);
    await ask(user);
    await user.click(screen.getByRole("button", { name: "Draft" }));

    await user.click(await screen.findByRole("button", { name: "Discard" }));

    expect(screen.queryByRole("heading", { name: /^Draft ready/ })).not.toBeInTheDocument();
    expect(onAccept).not.toHaveBeenCalled();
  });

  it("says plainly when the budget is spent, the limit is hit or the provider fails", async () => {
    const cases: [number, string, string, string][] = [
      [402, "ai_budget_exceeded", "Payment Required", "The monthly AI budget is spent"],
      [429, "ai_rate_limited", "Too Many Requests", "Too many AI requests"],
      [502, "ai_provider_error", "Bad Gateway", "The model could not be reached"],
      [409, "ai_disabled", "Conflict", "The AI assistant is off"],
    ];
    for (const [status, code, title, shown] of cases) {
      mockApi({
        ...BASE,
        [SETTINGS]: () => json(settings(true)),
        [DRAFT]: () =>
          new Response(JSON.stringify({ title, status, detail: "x", code }), {
            status,
            headers: { "content-type": "application/problem+json" },
          }),
      });
      const user = userEvent.setup();
      const view = mount();
      await ask(user);
      await user.click(screen.getByRole("button", { name: "Draft" }));

      expect(await screen.findByRole("alert")).toHaveTextContent(shown);
      view.unmount();
    }
  });

  it("waits for a webhook sample that parses before it drafts", async () => {
    const calls = mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [DRAFT]: () => json(VALID) });
    const user = userEvent.setup();
    const waiting = mount(() => undefined, null);
    await ask(user);
    expect(screen.getByRole("button", { name: "Draft" })).toBeDisabled();
    waiting.unmount();

    mount(() => undefined, [{ email: "a@example.com" }]);
    await ask(user);
    await user.click(screen.getByRole("button", { name: "Draft" }));

    await screen.findByRole("heading", { name: /^Draft ready/ });
    const sent = calls.find((call) => call.path.endsWith("/ai/drafts"));
    expect(sent?.body).toMatchObject({ sample: [{ email: "a@example.com" }] });
  });
});

const SEGMENT_CONTENT = { v: 1, all: [{ trait: "city", op: "eq", value: "Odense" }] };

const SEGMENT_VALID = {
  id: "d1",
  kind: "segment",
  status: "valid",
  attempts: 1,
  content: SEGMENT_CONTENT,
  issues: [],
  changes: [{ path: "/all", change: "changed", before: [{ trait: "city", op: "eq", value: "Aarhus" }], after: SEGMENT_CONTENT.all }],
  dry_run: null,
};

const SEGMENT_INVALID = {
  id: "d2",
  kind: "segment",
  status: "invalid",
  attempts: 3,
  content: null,
  issues: [{ code: "unknown_trait", at: "/all/0/trait" }],
  changes: [],
  dry_run: null,
};

function SegmentHarness({
  segmentId,
  onAccept,
  confirmAccept,
  renderPreview,
}: {
  segmentId?: string;
  onAccept: (content: unknown) => void;
  confirmAccept?: () => boolean | Promise<boolean>;
  renderPreview?: (content: unknown) => React.ReactNode;
}) {
  const { state } = useSession();
  if (state.status !== "signed-in") return <p>Waiting for the session</p>;
  return (
    <AssistantPanel
      workspaceId="ws1"
      kind="segment"
      segmentId={segmentId}
      onAccept={onAccept}
      confirmAccept={confirmAccept}
      renderPreview={renderPreview}
    />
  );
}

function mountSegment(props: Partial<Parameters<typeof SegmentHarness>[0]> = {}) {
  return render(
    <SessionProvider>
      <SegmentHarness onAccept={() => undefined} {...props} />
    </SessionProvider>,
  );
}

describe("the assistant panel for a segment", () => {
  it("is an empty state while the assistant is off", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(false)) });
    mountSegment();

    expect(await screen.findByText("The assistant is off")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Draft" })).not.toBeInTheDocument();
  });

  it("says what it sends, and what it keeps back, by the workspace opt-in", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true, false)) });
    const first = mountSegment();
    expect(await screen.findByText(/Values in the saved definition that could be personal are hidden from it/)).toBeInTheDocument();
    first.unmount();

    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true, true)) });
    mountSegment();
    expect(await screen.findByText(/and the values of the saved definition \(personal data is allowed/)).toBeInTheDocument();
  });

  it("asks for the segment being edited, or for none when the segment is new, and never for a source", async () => {
    const calls = mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [DRAFT]: () => json(SEGMENT_VALID) });
    const user = userEvent.setup();
    const saved = mountSegment({ segmentId: "seg-1" });
    await ask(user, "Odense instead");
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await screen.findByRole("heading", { name: /^Draft ready/ });
    saved.unmount();

    mountSegment();
    await ask(user, "Everyone in Odense");
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await screen.findByRole("heading", { name: /^Draft ready/ });

    const bodies = calls.filter((call) => call.path.endsWith("/ai/drafts")).map((call) => call.body);
    expect(bodies).toEqual([
      { kind: "segment", prompt: "Odense instead", segment_id: "seg-1" },
      { kind: "segment", prompt: "Everyone in Odense" },
    ]);
  });

  it("shows a loading state with Cancel, and nothing is kept when it is cancelled", async () => {
    mockApi({
      ...BASE,
      [SETTINGS]: () => json(settings(true)),
      [DRAFT]: (request) =>
        new Promise<Response>((_resolve, reject) => {
          request.signal.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")));
        }),
    });
    const user = userEvent.setup();
    mountSegment();
    await ask(user);

    await user.click(screen.getByRole("button", { name: "Draft" }));
    expect(await screen.findByText("Drafting, this can take a minute")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Cancel" }));

    expect(await screen.findByText("Cancelled. Nothing was kept.")).toBeInTheDocument();
  });

  it("shows a valid draft with its changes, focus on it and an announcement, and no dry run", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [DRAFT]: () => json(SEGMENT_VALID) });
    const user = userEvent.setup();
    mountSegment({ segmentId: "seg-1" });
    await ask(user);

    await user.click(screen.getByRole("button", { name: "Draft" }));

    expect(await screen.findByRole("heading", { name: /^Draft ready/ })).toHaveFocus();
    expect(screen.getByRole("table", { name: "What the draft changes" })).toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: "Dry run" })).not.toBeInTheDocument();
    expect(screen.getAllByRole("status").some((node) => node.textContent === "Draft ready")).toBe(true);
  });

  it("renders the host preview of a valid draft, with the content of the draft", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [DRAFT]: () => json(SEGMENT_VALID) });
    const renderPreview = vi.fn((content: unknown) => <p>{`Would match ${JSON.stringify(content)}`}</p>);
    const user = userEvent.setup();
    mountSegment({ renderPreview });
    await ask(user);

    await user.click(screen.getByRole("button", { name: "Draft" }));

    expect(await screen.findByText(`Would match ${JSON.stringify(SEGMENT_CONTENT)}`)).toBeInTheDocument();
    expect(renderPreview).toHaveBeenCalledWith(SEGMENT_CONTENT, expect.objectContaining({ id: "d1", status: "valid" }));
  });

  it("renders no preview for a draft that never validated", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [DRAFT]: () => json(SEGMENT_INVALID) });
    const renderPreview = vi.fn(() => <p>Would match</p>);
    const user = userEvent.setup();
    mountSegment({ renderPreview });
    await ask(user);

    await user.click(screen.getByRole("button", { name: "Draft" }));

    const problems = await screen.findByRole("list", { name: "Problems with the last attempt" });
    expect(problems).toHaveTextContent("The definition uses a field that does not exist in this workspace. (/all/0/trait)");
    expect(screen.queryByRole("button", { name: "Put into the draft" })).not.toBeInTheDocument();
    expect(renderPreview).not.toHaveBeenCalled();
  });

  it("waits for a host that answers its question in a dialog, and accepts nothing on a no", async () => {
    const calls = mockApi({
      ...BASE,
      [SETTINGS]: () => json(settings(true)),
      [DRAFT]: () => json(SEGMENT_VALID),
      [ACCEPT]: () => json({ id: "d1", kind: "segment", segment_id: "seg-1", content: SEGMENT_CONTENT }),
    });
    const onAccept = vi.fn();
    let decide: (replace: boolean) => void = () => undefined;
    const confirmAccept = vi.fn(
      () =>
        new Promise<boolean>((resolve) => {
          decide = resolve;
        }),
    );
    const user = userEvent.setup();
    mountSegment({ segmentId: "seg-1", onAccept, confirmAccept });
    await ask(user);
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await user.click(await screen.findByRole("button", { name: "Put into the draft" }));

    expect(confirmAccept).toHaveBeenCalledTimes(1);
    expect(calls.some((call) => call.path.endsWith("/accept"))).toBe(false);
    decide(false);

    await waitFor(() => expect(screen.getByRole("button", { name: "Put into the draft" })).toBeEnabled());
    expect(calls.some((call) => call.path.endsWith("/accept"))).toBe(false);
    expect(onAccept).not.toHaveBeenCalled();

    await user.click(screen.getByRole("button", { name: "Put into the draft" }));
    decide(true);

    expect(await screen.findByText("Loaded into the editor. Save to apply it.")).toBeInTheDocument();
    expect(onAccept).toHaveBeenCalledWith(SEGMENT_CONTENT, expect.objectContaining({ id: "d1" }));
  });

  it("names why an accept was refused: the saved version changed, or the draft no longer validates", async () => {
    const refused = (code: string, issues: unknown[] = []) =>
      new Response(JSON.stringify({ title: "Conflict", status: 409, detail: "x", code, issues }), {
        status: 409,
        headers: { "content-type": "application/problem+json" },
      });
    const cases: [string, unknown[], string][] = [
      ["ai_draft_outdated", [], "The saved version changed"],
      [
        "ai_draft_invalid",
        [{ pointer: "/all/0/trait", code: "unknown_trait" }],
        "The definition uses a field that does not exist in this workspace. (/all/0/trait)",
      ],
    ];
    for (const [code, issues, shown] of cases) {
      mockApi({
        ...BASE,
        [SETTINGS]: () => json(settings(true)),
        [DRAFT]: () => json(SEGMENT_VALID),
        [ACCEPT]: () => refused(code, issues),
      });
      const user = userEvent.setup();
      const view = mountSegment({ segmentId: "seg-1" });
      await ask(user);
      await user.click(screen.getByRole("button", { name: "Draft" }));

      await user.click(await screen.findByRole("button", { name: "Put into the draft" }));

      expect(await screen.findByRole("alert")).toHaveTextContent(shown);
      expect(screen.getByRole("button", { name: "Put into the draft" })).toBeEnabled();
      view.unmount();
    }
  });

  it("says plainly when the budget is spent, the limit is hit or the provider fails", async () => {
    const cases: [number, string, string, string][] = [
      [402, "ai_budget_exceeded", "Payment Required", "The monthly AI budget is spent"],
      [429, "ai_rate_limited", "Too Many Requests", "Too many AI requests"],
      [502, "ai_provider_error", "Bad Gateway", "The model could not be reached"],
    ];
    for (const [status, code, title, shown] of cases) {
      mockApi({
        ...BASE,
        [SETTINGS]: () => json(settings(true)),
        [DRAFT]: () =>
          new Response(JSON.stringify({ title, status, detail: "x", code }), {
            status,
            headers: { "content-type": "application/problem+json" },
          }),
      });
      const user = userEvent.setup();
      const view = mountSegment();
      await ask(user);
      await user.click(screen.getByRole("button", { name: "Draft" }));

      expect(await screen.findByRole("alert")).toHaveTextContent(shown);
      view.unmount();
    }
  });

  it("discards a draft without accepting it", async () => {
    const onAccept = vi.fn();
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [DRAFT]: () => json(SEGMENT_VALID) });
    const user = userEvent.setup();
    mountSegment({ onAccept });
    await ask(user);
    await user.click(screen.getByRole("button", { name: "Draft" }));

    await user.click(await screen.findByRole("button", { name: "Discard" }));

    expect(screen.queryByRole("heading", { name: /^Draft ready/ })).not.toBeInTheDocument();
    expect(onAccept).not.toHaveBeenCalled();
  });
});

const USAGE = "GET /api/v1/workspaces/ws1/ai/usage";

function usage(spent: number, budget: number) {
  return { budget_micros: budget, spent_micros: spent, by_feature: [], by_user: [], month_start: "2026-10-01T00:00:00Z", recent: [] };
}

describe("the footer and the draft card of the assistant dock", () => {
  it("shows the request, a card counting the changes, the check line and the footnote", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [USAGE]: () => json(usage(1_840_000, 20_000_000)), [DRAFT]: () => json(VALID) });
    const user = userEvent.setup();
    mount();
    await ask(user, "Map the email");
    await user.click(screen.getByRole("button", { name: "Draft" }));

    expect(await screen.findByRole("heading", { name: "Draft ready · 2 changes" })).toBeInTheDocument();
    expect(screen.getByText("Map the email", { selector: ".dock-request" })).toBeInTheDocument();
    expect(screen.getByText("Passes the checks")).toBeInTheDocument();
    expect(screen.getByText("Nothing goes live until you review and publish.")).toBeInTheDocument();
    expect(screen.getByLabelText("Ask for another change")).toBeInTheDocument();
  });

  it("shows what has been spent of the monthly budget beside the Draft button", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [USAGE]: () => json(usage(1_840_000, 20_000_000)) });
    mount();

    expect(await screen.findByText("1.84 of 20.00 used this month")).toBeInTheDocument();
  });

  it("disables the prompt and says so once the monthly budget is used", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [USAGE]: () => json(usage(20_000_000, 20_000_000)) });
    mount();

    expect(await screen.findByText("Monthly budget used")).toBeInTheDocument();
    expect(screen.getByLabelText("Ask for a change")).toBeDisabled();
    expect(screen.getByRole("button", { name: "Draft" })).toBeDisabled();
  });

  it("tells the host which valid draft is showing, and none once it is discarded", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [DRAFT]: () => json(VALID) });
    const shown = vi.fn();
    const user = userEvent.setup();
    render(
      <SessionProvider>
        <ShownHarness onDraftShown={shown} />
      </SessionProvider>,
    );
    await user.type(await screen.findByLabelText("Ask for a change"), "Map the email");
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await user.click(await screen.findByRole("button", { name: "Discard" }));

    expect(shown.mock.calls.map((call) => (call[0] as { id: string } | null)?.id ?? null)).toEqual([null, "d1", null]);
  });
});

function ShownHarness({ onDraftShown }: { onDraftShown: (draft: unknown) => void }) {
  const { state } = useSession();
  if (state.status !== "signed-in") return <p>Waiting for the session</p>;
  return <AssistantPanel workspaceId="ws1" kind="mapping" sourceId="src-1" stream="contacts" onAccept={() => undefined} onDraftShown={onDraftShown} />;
}

describe("what the assistant may call", () => {
  it("only reads settings and usage, drafts, and accepts a draft into the editor", async () => {
    const calls = mockApi({
      ...BASE,
      [SETTINGS]: () => json(settings(true)),
      [USAGE]: () => json(usage(0, 20_000_000)),
      [DRAFT]: () => json(VALID),
      [ACCEPT]: () => json({ id: "d1", kind: "mapping", source_id: "src-1", stream: "contacts", content: CONTENT }),
    });
    const user = userEvent.setup();
    mount();
    await ask(user);
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await user.click(await screen.findByRole("button", { name: "Put into the draft" }));
    await screen.findByText("Loaded into the editor. Save to apply it.");

    const used = new Set(calls.filter((call) => call.path.includes("/ai/")).map((call) => `${call.method} ${call.path}`));
    expect([...used].sort()).toEqual([ACCEPT, DRAFT, SETTINGS, USAGE].sort());
    expect(calls.filter((call) => !call.path.includes("/ai/") && call.path !== "/api/v1/session")).toEqual([]);
  });

  it("has no publish, send, delete or consent call anywhere in its code", () => {
    const sources = import.meta.glob<string>(["./*.ts", "./*.tsx", "!./*.test.ts", "!./*.test.tsx", "!./AiSettings*", "!./UsagePanel.tsx"], {
      query: "?raw",
      import: "default",
      eager: true,
    });
    const requests = Object.values(sources).flatMap((code) => code.match(/client\.(GET|POST|PUT|PATCH|DELETE)\(\s*"[^"]+"/g) ?? []);

    expect(requests.length).toBeGreaterThan(0);
    for (const request of requests) {
      expect(request).not.toMatch(/\.(PUT|PATCH|DELETE)\(/);
      expect(request).not.toMatch(/publish|send|consent|archive|delete/i);
    }
  });
});
