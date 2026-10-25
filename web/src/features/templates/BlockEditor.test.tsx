import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { drafts, previewBody, templateDetail, templateRoutes, SENDER_ID, TEMPLATE_ID, WS, type Doc, type Issue } from "../../test/templatesApi";
import { TemplatePage } from "./TemplatePage";
import { DEFAULT_STYLE } from "./blocks/document";

const WAIT = { timeout: 4000 };
const BASE = `${WS}/templates/${TEMPLATE_ID}`;

function mount() {
  return renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);
}

function blocksDocument(sections: unknown[] = [], overrides: Doc = {}, schema = 1): Doc {
  return {
    schema,
    channel: "email",
    mode: "blocks",
    blocks: { subject: "Hello", preheader: "", category: "marketing", sender: SENDER_ID, settings: { ...DEFAULT_STYLE }, sections },
    ...overrides,
  };
}

function detailOf(document: Doc) {
  return { ...templateDetail({}, document), draft: { ...templateDetail({}, document).draft, content_type: "blocks" } };
}

const heading = (id: string, text: string) => ({ type: "heading", id, level: 2, text });
const divider = (id: string) => ({ type: "divider", id });
const spacer = (id: string) => ({ type: "spacer", id, height: 20 });

function lastDraft(calls: ReturnType<typeof mockApi>): { blocks: { sections: { id: string; columns: { id: string }[][] }[] } } & Doc {
  const all = drafts(calls);
  return all[all.length - 1] as never;
}

function ids(calls: ReturnType<typeof mockApi>): string[] {
  return lastDraft(calls).blocks.sections.flatMap((section) => section.columns.flatMap((column) => column.map((block) => block.id)));
}

function blocking(code: string, at: string): Issue {
  return { code, severity: "blocking", at };
}

describe("building with blocks", () => {
  it("shows an empty state for a document with no blocks", async () => {
    mockApi(templateRoutes({ detail: detailOf(blocksDocument()) }));
    mount();

    expect(await screen.findByText("No blocks yet")).toBeInTheDocument();
    expect(screen.getByText("Blocks template")).toBeInTheDocument();
  });

  it("inserts every block type, edits its settings and saves the v1 document", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument()) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Add a section with 1 column" }));

    await user.click(screen.getByRole("button", { name: "Add heading block" }));
    const text = await screen.findByRole("textbox", { name: "Heading text" });
    await user.clear(text);
    await user.type(text, "Welcome");
    await user.selectOptions(screen.getByRole("combobox", { name: "Level" }), "1");

    await user.click(screen.getByRole("button", { name: "Add text block" }));
    const rich = await screen.findByRole("textbox", { name: "Text" });
    rich.innerHTML = "<div>Hello <b>there</b></div><script>alert(1)</script>";
    fireEvent.input(rich);

    await user.click(screen.getByRole("button", { name: "Add image block" }));
    await user.click(await screen.findByRole("textbox", { name: "Image address" }));
    await user.paste("https://cdn.example/a.png");
    await user.click(screen.getByRole("textbox", { name: "Alt text" }));
    await user.paste("A logo");

    await user.click(screen.getByRole("button", { name: "Add button block" }));
    await user.click(await screen.findByRole("textbox", { name: "Label" }));
    await user.paste("!");
    await user.click(screen.getByRole("textbox", { name: "URL" }));
    await user.paste("https://shop.example");
    await user.selectOptions(screen.getByRole("combobox", { name: "Alignment" }), "center");

    await user.click(screen.getByRole("button", { name: "Add divider block" }));
    await user.click(await screen.findByRole("textbox", { name: "Line colour" }));
    await user.paste("#cccccc");

    await user.click(screen.getByRole("button", { name: "Add spacer block" }));
    const height = await screen.findByRole("spinbutton", { name: "Height in pixels" });
    await user.clear(height);
    await user.type(height, "40");

    await user.click(screen.getByRole("button", { name: "Add raw html block" }));
    await user.click(await screen.findByRole("textbox", { name: "HTML" }));
    await user.paste("<i>mine</i>");

    await waitFor(() => {
      expect(lastDraft(calls).blocks.sections[0]?.columns[0]).toHaveLength(7);
      expect(JSON.stringify(lastDraft(calls))).toContain("mine");
    }, WAIT);
    expect(lastDraft(calls)).toEqual({
      schema: 1,
      channel: "email",
      mode: "blocks",
      blocks: {
        subject: "Hello",
        preheader: "",
        category: "marketing",
        sender: SENDER_ID,
        settings: DEFAULT_STYLE,
        sections: [
          {
            id: "section-1",
            columns: [
              [
                { type: "heading", id: "heading-1", level: 1, text: "Welcome" },
                { type: "text", id: "text-1", content: "<p>Hello <b>there</b></p>" },
                { type: "image", id: "image-1", src: "https://cdn.example/a.png", alt: "A logo" },
                { type: "button", id: "button-1", label: "Button!", url: "https://shop.example", align: "center" },
                { type: "divider", id: "divider-1", color: "#cccccc" },
                { type: "spacer", id: "spacer-1", height: 40 },
                { type: "raw_html", id: "raw-html-1", html: "<i>mine</i>" },
              ],
            ],
          },
        ],
      },
    });
  });

  it("edits the document style and the message", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument()) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Document style" }));
    const width = screen.getByRole("spinbutton", { name: "Content width in pixels" });
    await user.clear(width);
    await user.type(width, "640");
    await user.selectOptions(screen.getByRole("combobox", { name: "Font" }), "georgia");
    await user.click(screen.getByRole("tab", { name: "Message" }));
    await user.click(screen.getByRole("textbox", { name: "Preheader" }));
    await user.paste("Short");

    await waitFor(() => {
      const blocks = (lastDraft(calls) as unknown as { blocks: { settings: Doc; preheader: string } }).blocks;
      expect(blocks.settings).toMatchObject({ content_width: 640, font_family: "georgia" });
      expect(blocks.preheader).toBe("Short");
    }, WAIT);
  });

  it("lets a writer who is not an admin pick the sender by name on the Message tab", async () => {
    mockApi(templateRoutes({ role: "member", detail: detailOf(blocksDocument()) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Message" }));

    expect(await screen.findByRole("option", { name: "Acme <hello@mail.acme.example>" })).toBeInTheDocument();
    expect(screen.getByLabelText("Sender identity")).toHaveValue(SENDER_ID);
  });

  it("puts a merge tag from the picker into the focused text field", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument([{ id: "s", columns: [[heading("h", "Hi ")]] }])) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("group", { name: "Heading block 1" }));
    const field = await screen.findByRole<HTMLInputElement>("textbox", { name: "Heading text" });
    await user.click(field);
    field.setSelectionRange(3, 3);
    await user.click(await screen.findByRole("button", { name: "Insert system.unsubscribe_url" }));

    await waitFor(() => expect(JSON.stringify(lastDraft(calls))).toContain("Hi {{ system.unsubscribe_url }}"), WAIT);
  });
});

describe("reordering", () => {
  const three = () => blocksDocument([{ id: "s", columns: [[heading("a", "A"), divider("b"), spacer("c")]] }]);

  it("moves a block up and down from the keyboard", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(three()) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Move Heading block 1 down" }));
    await waitFor(() => expect(ids(calls)).toEqual(["b", "a", "c"]), WAIT);
    await user.click(screen.getByRole("button", { name: "Move Spacer block 3 up" }));
    await waitFor(() => expect(ids(calls)).toEqual(["b", "c", "a"]), WAIT);
  });

  it("moves a block with Alt and the arrow keys on the focused block", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(three()) }));
    const user = userEvent.setup();
    mount();

    const block = await screen.findByRole("group", { name: "Divider block 2" });
    block.focus();
    await user.keyboard("{Alt>}{ArrowUp}{/Alt}");

    await waitFor(() => expect(ids(calls)).toEqual(["b", "a", "c"]), WAIT);
  });

  it("moves a block by drag and drop", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(three()) }));
    mount();

    const handle = await screen.findByRole("img", { name: /Drag handle for Heading block 1/ });
    fireEvent.dragStart(handle);
    const slot = screen.getByTestId("slot-column-0-0-3");
    expect(fireEvent.dragOver(slot)).toBe(false);
    fireEvent.drop(slot);

    await waitFor(() => expect(ids(calls)).toEqual(["b", "c", "a"]), WAIT);
  });

  it("does not let a section be dropped inside a section", async () => {
    const calls = mockApi(
      templateRoutes({
        detail: detailOf(
          blocksDocument([
            { id: "one", columns: [[heading("a", "A")]] },
            { id: "two", columns: [[divider("b")]] },
          ]),
        ),
      }),
    );
    mount();

    const handle = await screen.findByRole("img", { name: /Drag handle for Section 1/ });
    fireEvent.dragStart(handle);
    const inside = screen.getByTestId("slot-column-1-0-0");
    expect(fireEvent.dragOver(inside)).toBe(true);
    fireEvent.drop(inside);
    // Between the sections is allowed.
    fireEvent.dragStart(handle);
    const between = screen.getByTestId("slot-root-2");
    expect(fireEvent.dragOver(between)).toBe(false);
    fireEvent.drop(between);

    await waitFor(() => expect(drafts(calls)).toHaveLength(1), WAIT);
    const sections = lastDraft(calls).blocks.sections;
    expect(sections.map((section) => section.id)).toEqual(["two", "one"]);
    expect(sections.every((section) => section.columns.every((column) => column.every((block) => !("columns" in block))))).toBe(true);
  });

  it("changes nothing when a section is dropped inside a section", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument([{ id: "one", columns: [[heading("a", "A")]] }])) }));
    mount();

    const handle = await screen.findByRole("img", { name: /Drag handle for Section 1/ });
    fireEvent.dragStart(handle);
    fireEvent.drop(screen.getByTestId("slot-column-0-0-1"));

    await new Promise((resolve) => setTimeout(resolve, 700));
    expect(drafts(calls)).toHaveLength(0);
  });

  it("inserts from the palette by drag", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument([{ id: "s", columns: [[heading("a", "A")]] }])) }));
    mount();

    fireEvent.dragStart(await screen.findByRole("button", { name: "Add spacer block" }));
    fireEvent.drop(screen.getByTestId("slot-column-0-0-0"));

    await waitFor(() => expect(ids(calls)).toEqual(["spacer-1", "a"]), WAIT);
  });
});

describe("validation, preview and publish", () => {
  const withButton = () => blocksDocument([{ id: "s", columns: [[{ type: "button", id: "cta", label: "Go", url: "x", align: "left" }]] }]);

  it("shows a blocking issue on its block and disables publish", async () => {
    mockApi(
      templateRoutes({
        detail: detailOf(withButton()),
        preview: () => previewBody({ rendered: false, issues: [blocking("invalid_url", "/blocks/block/cta/url")] }),
      }),
    );
    mount();

    const block = await screen.findByRole("group", { name: "Button block 1" });
    expect(await within(block).findByText(/Blocks publishing: .*web address is not valid/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Publish…" })).toBeDisabled();
  });

  it("shows a warning on its block and still allows publish", async () => {
    mockApi(
      templateRoutes({
        detail: detailOf(withButton()),
        preview: () => previewBody({ issues: [{ code: "insecure_link", severity: "warning", at: "/blocks/block/cta/url" }] }),
      }),
    );
    mount();

    const block = await screen.findByRole("group", { name: "Button block 1" });
    expect(await within(block).findByText(/Warning: /)).toBeInTheDocument();
    await waitFor(() => expect(screen.getByRole("button", { name: "Publish…" })).toBeEnabled(), WAIT);
  });

  it("previews the server render in an iframe with exactly an empty sandbox", async () => {
    mockApi(templateRoutes({ detail: detailOf(withButton()), preview: () => previewBody({ html: "<script>window.__ran = true</script><p>x</p>" }) }));
    mount();

    const frame = await screen.findByTitle("Preview of the message");
    expect(frame.getAttribute("sandbox")).toBe("");
    expect(frame).toHaveAttribute("srcdoc", "<script>window.__ran = true</script><p>x</p>");
    expect(await screen.findByRole("button", { name: "Mobile" })).toBeInTheDocument();
    expect((window as unknown as Record<string, unknown>)["__ran"]).toBeUndefined();
  });
});

describe("a newer schema", () => {
  it("opens read-only with a notice and offers no editing", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument([{ id: "s", columns: [[heading("a", "A")]] }], {}, 2)) }));
    mount();

    expect(await screen.findByText(/written by a newer version of the editor/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add heading block" })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Remove / })).toBeNull();
    await new Promise((resolve) => setTimeout(resolve, 700));
    expect(drafts(calls)).toHaveLength(0);
  });
});

describe("converting to MJML", () => {
  it("asks for a confirmation that says it is one way, then calls the server and switches to code mode", async () => {
    const source = blocksDocument([{ id: "s", columns: [[heading("a", "Hi {{ person.city }}")]] }]);
    const converted = { ...detailOf(source), draft: { ...detailOf(source).draft, content_type: "mjml", revision: 2, document: { schema: 1, channel: "email", mode: "code", email: { subject: "Hello", html: "<mjml></mjml>", text: "", category: "marketing", syntax: "mjml" } } } };
    const calls = mockApi({
      ...templateRoutes({ detail: detailOf(source) }),
      [`POST ${BASE}/draft/convert-to-mjml`]: () => json(converted),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Template mode" }));
    await user.click(screen.getByRole("menuitem", { name: "Convert to MJML…" }));

    const dialog = await screen.findByRole("dialog", { name: "Convert to MJML code?" });
    expect(within(dialog).getByText(/This is one way/)).toBeInTheDocument();
    expect(calls.filter((call) => call.path.endsWith("convert-to-mjml"))).toHaveLength(0);

    await user.click(within(dialog).getByRole("button", { name: "Convert to MJML" }));

    await waitFor(() => expect(calls.filter((call) => call.method === "POST" && call.path.endsWith("/draft/convert-to-mjml"))).toHaveLength(1), WAIT);
    expect(await screen.findByRole("textbox", { name: "MJML source" })).toHaveValue("<mjml></mjml>");
    expect(screen.getByText("MJML template")).toBeInTheDocument();
  });

  it("cancelling the confirmation converts nothing", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument()) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Template mode" }));
    await user.click(screen.getByRole("menuitem", { name: "Convert to MJML…" }));
    await user.click(await screen.findByRole("button", { name: "Cancel" }));

    expect(calls.some((call) => call.path.endsWith("convert-to-mjml"))).toBe(false);
    expect(screen.queryByRole("dialog", { name: "Convert to MJML code?" })).toBeNull();
  });

  it("shows the server's refusal and stays in blocks mode", async () => {
    mockApi({
      ...templateRoutes({ detail: detailOf(blocksDocument()) }),
      [`POST ${BASE}/draft/convert-to-mjml`]: () => problem(422, "Unprocessable", "the template is not valid", undefined, "template_invalid"),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Template mode" }));
    await user.click(screen.getByRole("menuitem", { name: "Convert to MJML…" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Convert to MJML" }));

    expect(await screen.findByText(/the template is not valid/)).toBeInTheDocument();
    expect(screen.getByText("Blocks template")).toBeInTheDocument();
  });
});

describe("review fixes", () => {
  it("keeps focus on a block moved across columns, and on a neighbour after a remove", async () => {
    mockApi(templateRoutes({ detail: detailOf(blocksDocument([{ id: "s", columns: [[heading("a", "A")], [divider("b")]] }])) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Move Heading block 1 down" }));
    // The heading moved into the second column and was remounted; focus is still in it.
    const moved = await screen.findByRole("group", { name: "Heading block 1" });
    await waitFor(() => expect(moved.contains(document.activeElement)).toBe(true));

    await user.click(within(moved).getByRole("button", { name: "Remove Heading block 1" }));
    const rest = await screen.findByRole("group", { name: "Divider block 1" });
    await waitFor(() => expect(document.activeElement).toBe(rest));
  });

  it("does not send a height nobody typed when the field is emptied", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument([{ id: "s", columns: [[spacer("p")]] }])) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("group", { name: "Spacer block 1" }));
    const height = await screen.findByRole("spinbutton", { name: "Height in pixels" });
    await user.clear(height);
    await new Promise((resolve) => setTimeout(resolve, 700));
    expect(drafts(calls)).toHaveLength(0);
    expect(await screen.findByText(/Enter a number from 1 to 200/)).toBeInTheDocument();

    await user.type(height, "30");
    await waitFor(() => expect(JSON.stringify(lastDraft(calls))).toContain('"height":30'), WAIT);
  });

  it("saves a pending edit before the conversion and no blocks document after it", async () => {
    const source = blocksDocument([{ id: "s", columns: [[heading("a", "A")]] }]);
    const code = { schema: 1, channel: "email", mode: "code", email: { subject: "Hello", html: "<mjml></mjml>", text: "", category: "marketing", syntax: "mjml" } };
    const calls = mockApi({
      ...templateRoutes({ detail: detailOf(source) }),
      [`POST ${BASE}/draft/convert-to-mjml`]: () => json({ ...detailOf(source), draft: { ...detailOf(source).draft, content_type: "mjml", document: code } }),
    });
    const user = userEvent.setup();
    mount();

    const text = await screen.findByRole("group", { name: "Heading block 1" });
    await user.click(text);
    await user.click(await screen.findByRole("textbox", { name: "Heading text" }));
    await user.paste("!");
    await user.click(screen.getByRole("button", { name: "Template mode" }));
    await user.click(screen.getByRole("menuitem", { name: "Convert to MJML…" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Convert to MJML" }));
    await screen.findByRole("textbox", { name: "MJML source" });
    await new Promise((resolve) => setTimeout(resolve, 900));

    const writes = calls.filter((call) => (call.method === "PUT" && call.path.endsWith("/draft")) || call.path.endsWith("/convert-to-mjml"));
    const at = writes.findIndex((call) => call.path.endsWith("/convert-to-mjml"));
    expect(at).toBeGreaterThan(0);
    expect(JSON.stringify(writes[at - 1]?.body)).toContain("A!");
    expect(writes.slice(at + 1).filter((call) => JSON.stringify(call.body).includes('"mode":"blocks"'))).toEqual([]);
  });

  it("offers no conversion for a document of a newer schema", async () => {
    mockApi(templateRoutes({ detail: detailOf(blocksDocument([], {}, 2)) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Template mode" }));
    expect(screen.queryByRole("menuitem", { name: /Convert to MJML/ })).toBeNull();
  });
});

describe("browser-check fixes", () => {
  it("enables Down on the last block when an empty column follows, and moves it there", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument([{ id: "s", columns: [[heading("a", "A"), divider("b")], []] }])) }));
    const user = userEvent.setup();
    mount();

    const down = await screen.findByRole("button", { name: "Move Divider block 2 down" });
    expect(down).toBeEnabled();
    expect(screen.getByRole("button", { name: "Move Heading block 1 up" })).toBeDisabled();
    await user.click(down);

    await waitFor(() => expect(lastDraft(calls).blocks.sections[0]?.columns.map((column) => column.map((block) => block.id))).toEqual([["a"], ["b"]]), WAIT);
  });

  it("enables Up on the first block when the column before it is empty", async () => {
    mockApi(templateRoutes({ detail: detailOf(blocksDocument([{ id: "s", columns: [[], [heading("a", "A")]] }])) }));
    mount();

    expect(await screen.findByRole("button", { name: "Move Heading block 1 up" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Move Heading block 1 down" })).toBeDisabled();
  });

  it("opens the Template mode menu from its left edge", async () => {
    mockApi(templateRoutes({ detail: detailOf(blocksDocument()) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Template mode" }));
    expect(screen.getByRole("menu", { name: "Template mode" })).toHaveClass("menu-start");
  });
});
