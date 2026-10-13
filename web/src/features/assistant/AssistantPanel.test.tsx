import { render, screen, within } from "@testing-library/react";
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

function Harness({ onAccept, sample }: { onAccept: (content: unknown) => void; sample?: unknown[] | null }) {
  const { state } = useSession();
  if (state.status !== "signed-in") return <p>Waiting for the session</p>;
  return (
    <AssistantPanel workspaceId="ws1" kind="mapping" sourceId="src-1" stream="contacts" sample={sample} onAccept={onAccept} />
  );
}

function mount(onAccept: (content: unknown) => void = () => undefined, sample?: unknown[] | null) {
  return render(
    <SessionProvider>
      <Harness onAccept={onAccept} sample={sample} />
    </SessionProvider>,
  );
}

async function ask(user: ReturnType<typeof userEvent.setup>, text = "Map the email") {
  await user.type(await screen.findByLabelText("What do you want?"), text);
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
    const area = await screen.findByLabelText("What do you want?");
    expect(screen.getByRole("button", { name: "Draft" })).toBeDisabled();

    await user.type(area, "Map the email{Control>}{Enter}{/Control}");

    expect(await screen.findByRole("heading", { name: "Draft ready" })).toBeInTheDocument();
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
    expect(screen.queryByRole("button", { name: "Accept" })).not.toBeInTheDocument();
    expect(screen.getByRole("status", { name: "" })).toHaveTextContent("No valid draft");
  });

  it("shows a valid draft with its changes and dry run, moves focus to it and announces it", async () => {
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [DRAFT]: () => json(VALID) });
    const user = userEvent.setup();
    mount();
    await ask(user);

    await user.click(screen.getByRole("button", { name: "Draft" }));

    expect(await screen.findByRole("heading", { name: "Draft ready" })).toHaveFocus();
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

    await user.click(await screen.findByRole("button", { name: "Accept" }));

    expect(await screen.findByText("Loaded into the editor. Save to apply it.")).toBeInTheDocument();
    expect(onAccept).toHaveBeenCalledWith(CONTENT);
    expect(calls.some((call) => call.method === "PUT")).toBe(false);
    expect(screen.queryByRole("button", { name: "Accept" })).not.toBeInTheDocument();
  });

  it("discards a draft without accepting it", async () => {
    const onAccept = vi.fn();
    mockApi({ ...BASE, [SETTINGS]: () => json(settings(true)), [DRAFT]: () => json(VALID) });
    const user = userEvent.setup();
    mount(onAccept);
    await ask(user);
    await user.click(screen.getByRole("button", { name: "Draft" }));

    await user.click(await screen.findByRole("button", { name: "Discard" }));

    expect(screen.queryByRole("heading", { name: "Draft ready" })).not.toBeInTheDocument();
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

    await screen.findByRole("heading", { name: "Draft ready" });
    const sent = calls.find((call) => call.path.endsWith("/ai/drafts"));
    expect(sent?.body).toMatchObject({ sample: [{ email: "a@example.com" }] });
  });
});
