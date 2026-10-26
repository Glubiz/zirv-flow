import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem, type Call, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { drafts, emailDocument, SENDER_ID, templateDetail, templateRoutes, TEMPLATE_ID, WS } from "../../test/templatesApi";
import { TemplatePage } from "./TemplatePage";
import { startingDocument } from "./model";

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
      issues: [{ code: "unknown_merge_field", at: "/email/html" }],
      changes: [],
      subject_variants: [],
    };
    mount(templateRoutes({ extra: { [DRAFTS]: () => json(answer) } }));
    const user = await open();
    await user.type(await screen.findByLabelText("What do you want?"), "Warmer");
    await user.click(screen.getByRole("button", { name: "Draft" }));

    const list = await screen.findByRole("list", { name: "Problems with the last attempt" });
    expect(list).toHaveTextContent("not a field in this workspace");
    expect(list).toHaveTextContent("/email/html");
    expect(screen.queryByRole("button", { name: /Accept/ })).not.toBeInTheDocument();

    answer = valid({ attempts: 2 });
    await user.click(screen.getByRole("button", { name: "Draft again" }));
    expect(await screen.findByText("Repaired before you saw it")).toBeInTheDocument();
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

  it("asks before replacing unsaved edits and accepts nothing when they are kept", async () => {
    const calls = mount(
      templateRoutes({
        extra: {
          [DRAFTS]: () => json(valid()),
          [ACCEPT]: () => problem(500, "Boom", "x"),
          [`PUT ${WS}/templates/${TEMPLATE_ID}/draft`]: () => new Promise<Response>(() => undefined),
        },
      }),
    );
    const user = userEvent.setup();
    await user.type(await screen.findByRole("textbox", { name: /source/ }), "x");
    await screen.findByText("Saving", undefined, WAIT);
    await user.click(screen.getByRole("button", { name: "Assistant" }));
    await user.type(await screen.findByLabelText("What do you want?"), "Warmer");
    await user.click(screen.getByRole("button", { name: "Draft" }));
    await screen.findByTitle("After render");
    await user.click(screen.getByRole("button", { name: "Accept into draft" }));

    expect(await screen.findByRole("dialog", { name: "Replace your unsaved changes?" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Keep my changes" }));
    expect(calls.some((call) => call.path.endsWith("/accept"))).toBe(false);
  });

  it("offers a reload when the draft changed since", async () => {
    const calls = mount(
      templateRoutes({
        extra: {
          [DRAFTS]: () => json(valid()),
          [ACCEPT]: () => problem(409, "Draft changed", "moved", undefined, "template_draft_changed"),
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
  });

  it("shows the notice and no button when the assistant is off", async () => {
    mockApi({ ...templateRoutes(), [`GET ${WS}/ai/settings`]: () => json(settings(false)) });
    renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);

    await screen.findByRole("textbox", { name: /source/ });
    expect(screen.queryByRole("button", { name: "Assistant" })).not.toBeInTheDocument();
  });
});
