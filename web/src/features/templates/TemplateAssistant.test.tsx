import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { json, mockApi, OWNER, problem, session, type Call, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { drafts, emailDocument, SENDER_ID, templateDetail, templateRoutes, TEMPLATE_ID, WS } from "../../test/templatesApi";
import { DEBOUNCE_MS } from "./TemplateEditor";
import { TemplateAssistant } from "./TemplateAssistant";
import { TemplatePage } from "./TemplatePage";
import { readDocument, startingDocument } from "./model";

const DRAFTS = `POST ${WS}/ai/drafts`;
const ACCEPT = `POST ${WS}/ai/drafts/d1/accept`;
const WAIT = { timeout: 4000 };

function settings(enabled = true) {
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

const RENDERS = {
  before: { subject: "Hello", html: "<p>old copy</p>", text: "old copy" },
  after: { subject: "Hi there", html: "<p>new copy</p>", text: "new copy" },
};

function valid(overrides: Record<string, unknown> = {}) {
  return {
    id: "d1",
    kind: "template",
    status: "valid",
    attempts: 1,
    content: emailDocument({}, { subject: "Hi there" }),
    issues: [],
    changes: [{ path: "/email/subject", change: "changed", before: "Hello", after: "Hi there" }],
    subject_variants: [],
    renders: RENDERS,
    ...overrides,
  };
}

function blocksDetail() {
  const document = { ...startingDocument("email", "blocks", "marketing"), sender: SENDER_ID };
  return templateDetail({}, document);
}

function accepted() {
  const detail = templateDetail({}, emailDocument({}, { subject: "Hi there" }));
  (detail.draft as { revision: number }).revision = 5;
  return { id: "d1", kind: "template", template_id: TEMPLATE_ID, content: detail.draft.document, template: detail };
}

function mount(routes: Record<string, Handler>) {
  const calls = mockApi({ [`GET ${WS}/ai/settings`]: () => json(settings()), ...routes });
  renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);
  return calls;
}

async function open() {
  const user = userEvent.setup();
  await user.click(await screen.findByRole("button", { name: "Assistant" }));
  return user;
}

function asked(calls: Call[]) {
  return calls.filter((call) => call.method === "POST" && call.path.endsWith("/ai/drafts")).map((call) => call.body as Record<string, unknown>);
}

describe("the template assistant", () => {
  it("sends each of the four tasks and shows the diff and both renders", async () => {
    const calls = mount(templateRoutes({ detail: blocksDetail(), extra: { [DRAFTS]: () => json(valid()) } }));
    const user = await open();

    const expected: [string, string, string][] = [
      ["Rewrite copy", "copy", "Warmer"],
      ["Subject lines", "subject_variants", "Short"],
      ["Block layout", "block_layout", "Two columns"],
      ["Translate", "translation", "Keep brand names"],
    ];
    for (const [label, task, text] of expected) {
      await user.selectOptions(await screen.findByLabelText("Task"), label);
      if (task === "translation") await user.type(screen.getByLabelText("Translate to"), "Danish");
      await user.type(screen.getByLabelText(task === "translation" ? "Instructions" : "What do you want?"), text);
      await user.click(screen.getByRole("button", { name: /^Draft/ }));
      expect(await screen.findByRole("table", { name: "What the draft changes" })).toBeInTheDocument();
      expect(screen.getByTitle("Before render")).toHaveAttribute("srcdoc", "<p>old copy</p>");
      expect(screen.getByTitle("After render")).toHaveAttribute("srcdoc", "<p>new copy</p>");
      await user.clear(screen.getByLabelText(task === "translation" ? "Instructions" : "What do you want?"));
    }

    expect(asked(calls)).toEqual([
      { kind: "template", template_id: TEMPLATE_ID, task: "copy", prompt: "Warmer" },
      { kind: "template", template_id: TEMPLATE_ID, task: "subject_variants", prompt: "Short" },
      { kind: "template", template_id: TEMPLATE_ID, task: "block_layout", prompt: "Two columns" },
      { kind: "template", template_id: TEMPLATE_ID, task: "translation", prompt: "Translate into Danish. Keep brand names" },
    ]);
  });

  it("offers no block layout for a code email and no panel for anything else", async () => {
    mount(templateRoutes({ extra: { [DRAFTS]: () => json(valid()) } }));
    await open();

    const options = within(await screen.findByLabelText("Task")).getAllByRole("option");
    expect(options.map((option) => option.textContent)).toEqual(["Rewrite copy", "Subject lines", "Translate"]);
  });

  it("changes the frame width with the toggles and keeps the frames sandboxed", async () => {
    mount(templateRoutes({ extra: { [DRAFTS]: () => json(valid()) } }));
    const user = await open();
    await user.type(await screen.findByLabelText("What do you want?"), "Warmer");
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await screen.findByTitle("After render");

    const frames = () => [screen.getByTitle("Before render"), screen.getByTitle("After render")];
    expect(frames().map((frame) => frame.parentElement?.className)).toEqual(["tp-frame", "tp-frame"]);
    const width = within(screen.getByRole("group", { name: "Width" }));
    await user.click(width.getByRole("button", { name: "Mobile" }));
    expect(frames().map((frame) => frame.parentElement?.className)).toEqual(["tp-frame tp-frame-mobile", "tp-frame tp-frame-mobile"]);
    await user.click(width.getByRole("button", { name: "Desktop" }));
    expect(frames().map((frame) => frame.parentElement?.className)).toEqual(["tp-frame", "tp-frame"]);
    for (const frame of frames()) expect(frame).toHaveAttribute("sandbox", "");
  });

  it("names an unrepaired issue with its place and offers no accept; a repaired draft is marked", async () => {
    let answer: unknown = {
      id: "d2",
      kind: "template",
      status: "invalid",
      attempts: 3,
      content: null,
      issues: [{ code: "unknown_merge_field", at: "/email/html", field: "person.nope" }],
      changes: [],
      subject_variants: [],
    };
    mount(templateRoutes({ extra: { [DRAFTS]: () => json(answer) } }));
    const user = await open();
    await user.type(await screen.findByLabelText("What do you want?"), "Warmer");
    await user.click(screen.getByRole("button", { name: "Draft" }));

    const list = await screen.findByRole("list", { name: "Problems with the last attempt" });
    expect(list).toHaveTextContent("person.nope: This merge field is not a field in this workspace.");
    expect(list).toHaveTextContent("/email/html");
    expect(screen.queryByRole("button", { name: /Accept/ })).not.toBeInTheDocument();

    answer = valid({ attempts: 2, repaired: ["/email/subject"] });
    await user.click(screen.getByRole("button", { name: "Draft again" }));
    const row = (await screen.findByRole("table", { name: "What the draft changes" })).querySelector("tbody tr");
    expect(row).toHaveTextContent("Repaired");
    expect(screen.getByText(/repaired a field before you saw/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Accept into draft" })).toBeEnabled();
  });

  it("accepts into the editor's new draft, applies the picked subject and never publishes", async () => {
    const accepted = templateDetail({}, emailDocument({}, { subject: "First" }));
    (accepted.draft as { revision: number }).revision = 5;
    const calls = mount(
      templateRoutes({
        extra: {
          [DRAFTS]: () => json(valid({ subject_variants: ["First", "Second"] })),
          [ACCEPT]: () => json({ id: "d1", kind: "template", template_id: TEMPLATE_ID, content: accepted.draft.document, template: accepted }),
        },
      }),
    );
    const user = await open();
    await user.type(await screen.findByLabelText("What do you want?"), "Short");
    await user.selectOptions(screen.getByLabelText("Task"), "Subject lines");
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await user.click(await screen.findByRole("radio", { name: "Second" }));
    await user.click(screen.getByRole("button", { name: "Accept into draft" }));

    expect(await screen.findByText(/saved as the template's draft/)).toBeInTheDocument();
    await waitFor(() => expect(drafts(calls)).toHaveLength(1), WAIT);
    expect((drafts(calls)[0] as { email: { subject: string } }).email.subject).toBe("Second");
    expect(calls.some((call) => call.path.endsWith("/publish"))).toBe(false);
  });

  it("does not draft from an older version when the edits cannot be saved", async () => {
    const calls = mount(
      templateRoutes({
        extra: { [DRAFTS]: () => json(valid()), [`PUT ${WS}/templates/${TEMPLATE_ID}/draft`]: () => problem(500, "Boom", "x") },
      }),
    );
    const user = userEvent.setup();
    await user.type(await screen.findByRole("textbox", { name: /source/ }), "x");
    await user.click(screen.getByRole("button", { name: "Assistant" }));
    await user.type(await screen.findByLabelText("What do you want?"), "Warmer");
    await user.click(screen.getByRole("button", { name: "Draft" }));

    expect(await screen.findByText("Your latest changes could not be saved")).toBeInTheDocument();
    expect(asked(calls)).toHaveLength(0);
  });

  it("offers a reload when the draft changed since", async () => {
    const moved = templateDetail({}, emailDocument({}, { html: "<p>Saved by someone else</p>" }));
    (moved.draft as { revision: number }).revision = 4;
    let refused = false;
    const calls = mount(
      templateRoutes({
        extra: {
          [DRAFTS]: () => json(valid()),
          [ACCEPT]: () => {
            refused = true;
            return problem(409, "Draft changed", "moved", undefined, "template_draft_changed");
          },
          [`GET ${WS}/templates/${TEMPLATE_ID}`]: () => json(refused ? moved : templateDetail()),
        },
      }),
    );
    const user = await open();
    await user.type(await screen.findByLabelText("What do you want?"), "Warmer");
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await user.click(await screen.findByRole("button", { name: "Accept into draft" }));

    expect(await screen.findByText("The template's draft changed")).toBeInTheDocument();
    const before = calls.filter((call) => call.method === "GET" && call.path === `${WS}/templates/${TEMPLATE_ID}`).length;
    await user.click(screen.getByRole("button", { name: "Reload the draft" }));
    await waitFor(() =>
      expect(calls.filter((call) => call.method === "GET" && call.path === `${WS}/templates/${TEMPLATE_ID}`).length).toBeGreaterThan(before),
    );
    // The editor holds the draft that was reloaded.
    await user.click(screen.getByRole("button", { name: "Close the assistant" }));
    expect((await screen.findByRole<HTMLTextAreaElement>("textbox", { name: /source/ })).value).toBe("<p>Saved by someone else</p>");
  });

  it("shows why a reload failed", async () => {
    let refused = false;
    mount(
      templateRoutes({
        extra: {
          [DRAFTS]: () => json(valid()),
          [ACCEPT]: () => {
            refused = true;
            return problem(409, "Draft changed", "moved", undefined, "template_draft_changed");
          },
          [`GET ${WS}/templates/${TEMPLATE_ID}`]: () => (refused ? problem(500, "Could not read the template", "try later") : json(templateDetail())),
        },
      }),
    );
    const user = await open();
    await user.type(await screen.findByLabelText("What do you want?"), "Warmer");
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await user.click(await screen.findByRole("button", { name: "Accept into draft" }));
    await user.click(await screen.findByRole("button", { name: "Reload the draft" }));

    expect(await screen.findByText("Could not read the template")).toBeInTheDocument();
  });

  it("opens to the usual notice, with nothing to draft, when no provider is configured", async () => {
    mockApi({ ...templateRoutes(), [`GET ${WS}/ai/settings`]: () => json(settings(false)) });
    renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);
    await open();

    expect(await screen.findByText("The assistant is off")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Draft" })).not.toBeInTheDocument();
  });

  it("offers the assistant to an admin only", async () => {
    mockApi(templateRoutes({ role: "member" }));
    renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);

    await screen.findByRole("textbox", { name: /source/ });
    expect(screen.queryByRole("button", { name: "Assistant" })).not.toBeInTheDocument();
  });
});

describe("the assistant and the editor's saves", () => {
  it("saves the edits that are waiting before the draft request goes out", async () => {
    const calls = mount(templateRoutes({ extra: { [DRAFTS]: () => json(valid()) } }));
    const user = userEvent.setup();
    await user.type(await screen.findByRole("textbox", { name: /source/ }), "x");
    await user.click(screen.getByRole("button", { name: "Assistant" }));
    await user.type(await screen.findByLabelText("What do you want?"), "Warmer");
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await screen.findByTitle("After render");

    const order = calls.filter((call) => call.path.endsWith("/draft") || call.path.endsWith("/ai/drafts")).map((call) => call.method + " " + call.path.split("/").slice(-2).join("/"));
    expect(order[0]).toMatch(/^PUT /);
    expect(order[order.length - 1]).toBe("POST ai/drafts");
  });

  function host(overrides: Partial<Parameters<typeof TemplateAssistant>[0]> = {}) {
    const doc = readDocument(emailDocument());
    const hold = vi.fn(() => Promise.resolve());
    const props = {
      workspaceId: "ws1",
      templateId: TEMPLATE_ID,
      doc,
      unsaved: false,
      flush: () => Promise.resolve(true),
      hold,
      release: vi.fn(),
      onAccepted: vi.fn(),
      onReconcile: () => Promise.resolve(false),
      onReload: () => Promise.resolve(),
      onClose: vi.fn(),
      ...overrides,
    };
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      [`GET ${WS}/ai/settings`]: () => json(settings()),
      [DRAFTS]: () => json(valid()),
      [ACCEPT]: () => json({ id: "d1", kind: "template", template_id: TEMPLATE_ID, content: {}, template: templateDetail() }),
    });
    renderRoute("/w/ws1/t", "/w/:ws/t", <TemplateAssistant {...props} />);
    return { calls, props };
  }

  async function draftOnce() {
    const user = userEvent.setup();
    await user.type(await screen.findByLabelText("What do you want?"), "Warmer");
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await user.click(await screen.findByRole("button", { name: "Accept into draft" }));
    return user;
  }

  it("cancels the queued save when unsaved edits are replaced, and accepts", async () => {
    const calls = mount(templateRoutes({ extra: { [DRAFTS]: () => json(valid()), [ACCEPT]: () => json(accepted()) } }));
    const user = await open();
    await user.type(await screen.findByLabelText("What do you want?"), "Warmer");
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await screen.findByTitle("After render");
    // Look at the editor, make an edit (its save is queued behind the debounce), and come back.
    await user.click(screen.getByRole("button", { name: "Assistant" }));
    await user.type(await screen.findByRole("textbox", { name: /source/ }), "x");
    await user.click(screen.getByRole("button", { name: "Assistant" }));
    await user.click(screen.getByRole("button", { name: "Accept into draft" }));
    await user.click(await screen.findByRole("button", { name: "Replace them" }));

    expect(await screen.findByText(/saved as the template's draft/)).toBeInTheDocument();
    // Past the debounce: the queued save never goes out.
    await new Promise((resolve) => setTimeout(resolve, DEBOUNCE_MS + 300));
    expect(calls.some((call) => call.method === "PUT" && call.path.endsWith("/draft"))).toBe(false);
    expect(calls.some((call) => call.path.endsWith("/accept"))).toBe(true);
  });

  it("waits for a save that is in flight before it accepts", async () => {
    let finish: () => void = () => undefined;
    const held = new Promise<void>((resolve) => {
      finish = resolve;
    });
    const { calls } = host({ hold: () => held });
    await draftOnce();

    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(calls.some((call) => call.path.endsWith("/accept"))).toBe(false);
    finish();
    await waitFor(() => expect(calls.some((call) => call.path.endsWith("/accept"))).toBe(true));
  });
});

describe("the prompt counter of a translation", () => {
  it("counts the text that is sent, so the counter and the Draft button agree", async () => {
    mount(templateRoutes());
    const user = await open();
    await user.selectOptions(await screen.findByLabelText("Task"), "Translate");
    await user.type(screen.getByLabelText("Translate to"), "Danish");
    await user.type(screen.getByLabelText("Instructions"), "abc");
    expect(screen.getByText(/^26 of 2000 characters/)).toBeInTheDocument();

    await user.clear(screen.getByLabelText("Instructions"));
    await user.click(screen.getByLabelText("Instructions"));
    await user.paste("x".repeat(1990));
    expect(screen.getByText(/^2013 of 2000 characters/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Draft" })).toBeDisabled();
  });
});

describe("a lost accept reply", () => {
  it("keeps the subject line that was picked", async () => {
    const calls = mockApi({
      ...templateRoutes({ extra: { [DRAFTS]: () => json(valid({ subject_variants: ["First", "Second"] })) } }),
      [`GET ${WS}/ai/settings`]: () => json(settings()),
    });
    // The server wrote the draft whose reply was lost: the next read returns it.
    const stored = templateDetail({}, emailDocument({}, { subject: "First" }));
    (stored.draft as { revision: number }).revision = 7;
    const original = globalThis.fetch;
    let accepted = false;
    vi.stubGlobal("fetch", async (input: Request | string | URL) => {
      const request = input instanceof Request ? input : new Request(input);
      if (request.method === "POST" && request.url.endsWith("/accept")) {
        accepted = true;
        throw new TypeError("network down");
      }
      if (accepted && request.method === "GET" && request.url.endsWith(`/templates/${TEMPLATE_ID}`)) return json(stored);
      return original(input);
    });
    renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Assistant" }));
    await user.type(await screen.findByLabelText("What do you want?"), "Short");
    await user.selectOptions(screen.getByLabelText("Task"), "Subject lines");
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await user.click(await screen.findByRole("radio", { name: "Second" }));
    await user.click(screen.getByRole("button", { name: "Accept into draft" }));

    await screen.findAllByText(/reply was lost/, undefined, WAIT);
    await waitFor(() => expect(drafts(calls).some((doc) => (doc as { email?: { subject?: string } }).email?.subject === "Second")).toBe(true), WAIT);
  });
});

describe("an assistant that is turned off while the panel is open", () => {
  it("shows the off notice after a refusal", async () => {
    let on = true;
    mount(
      templateRoutes({
        extra: {
          [`GET ${WS}/ai/settings`]: () => json(settings(on)),
          [DRAFTS]: () => {
            on = false;
            return problem(409, "The AI assistant is off", "off", undefined, "ai_disabled");
          },
        },
      }),
    );
    const user = await open();
    await user.type(await screen.findByLabelText("What do you want?"), "Warmer");
    await user.click(screen.getByRole("button", { name: "Draft" }));

    expect(await screen.findByText("The assistant is off")).toBeInTheDocument();
  });
});
