import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";

import { assetBody, listBody } from "../../test/assetsApi";
import { json, mockApi, problem, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { snippetBody } from "../../test/snippetsApi";
import { listItem, tokens } from "../../test/themesApi";
import { drafts, previewBody, templateDetail, templateRoutes, SENDER_ID, TEMPLATE_ID, WS, type Doc, type Issue } from "../../test/templatesApi";
import { TemplatePage } from "./TemplatePage";
import { DEFAULT_STYLE } from "./blocks/document";
import { readDocument, startingDocument, writeDocument } from "./model";

const WAIT = { timeout: 4000 };
const BASE = `${WS}/templates/${TEMPLATE_ID}`;

function mount() {
  return renderRoute(`/w/ws1/templates/${TEMPLATE_ID}`, "/w/:ws/templates/:id", <TemplatePage />);
}

function blocksDocument(sections: unknown[] = [], overrides: Doc = {}, schema = 2, extra: Doc = {}): Doc {
  return {
    schema,
    channel: "email",
    mode: "blocks",
    blocks: { subject: "Hello", preheader: "", category: "marketing", sender: SENDER_ID, settings: { ...DEFAULT_STYLE }, sections, ...extra },
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

  it("inserts every block type, edits its settings and saves the schema 2 document", async () => {
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
    await user.click(await screen.findByRole("button", { name: "Choose image…" }));
    await user.click(await screen.findByRole("button", { name: "Use a web address" }));
    await user.click(screen.getByRole("textbox", { name: "Web address" }));
    await user.paste("https://cdn.example/a.png");
    await user.click(screen.getByRole("button", { name: "Use image" }));
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
      schema: 2,
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
                { type: "image", id: "image-1", src: { url: "https://cdn.example/a.png" }, alt: "A logo" },
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

  it("edits the document style of a document with no theme, and the message", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument([], {}, 2, { theme: "none", footer: "none" })) }));
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
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument([{ id: "s", columns: [[heading("a", "A")]] }], {}, 3)) }));
    mount();

    expect(await screen.findByText(/written by a newer version of the editor/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add heading block" })).toBeNull();
    expect(screen.queryByRole("button", { name: /^Remove / })).toBeNull();
    await new Promise((resolve) => setTimeout(resolve, 700));
    expect(drafts(calls)).toHaveLength(0);
  });
});

describe("a schema 2 document", () => {
  const rich = (): Doc =>
    blocksDocument(
      [
        {
          id: "s",
          visible_if: { all: [] },
          columns: [
            [
              { type: "heading", id: "h", level: 2, text: "Hi", visible_if: { any: [1] } },
              { type: "image", id: "i", src: { asset: "0192b1a4-7c3e-7000-8000-00000000b001" }, alt: "Logo" },
              { type: "snippet", id: "n", snippet_id: "0192b1a4-7c3e-7000-8000-00000000c001" },
            ],
          ],
        },
      ],
      {},
      2,
    );

  it("carries the theme, the footer, conditions and snippet blocks through an edit untouched", async () => {
    const document = rich();
    (document["blocks"] as Doc)["theme"] = { id: "0192b1a4-7c3e-7000-8000-00000000d001" };
    (document["blocks"] as Doc)["footer"] = "none";
    const calls = mockApi(templateRoutes({ detail: detailOf(document) }));
    const user = userEvent.setup();
    mount();

    expect(await screen.findByText(/Snippet 0192b1a4-7c3e-7000-8000-00000000c001/)).toBeInTheDocument();
    await user.click(await screen.findByRole("group", { name: "Heading block 1" }));
    const field = await screen.findByRole("textbox", { name: "Heading text" });
    await user.click(field);
    await user.paste("!");

    await waitFor(() => expect(JSON.stringify(lastDraft(calls))).toContain("Hi!"), WAIT);
    const saved = lastDraft(calls) as unknown as { schema: number; blocks: Doc & { sections: { visible_if: unknown; columns: Doc[][] }[] } };
    expect(saved.schema).toBe(2);
    expect(saved.blocks["theme"]).toEqual({ id: "0192b1a4-7c3e-7000-8000-00000000d001" });
    expect(saved.blocks["footer"]).toBe("none");
    expect(saved.blocks.sections[0]?.visible_if).toEqual({ all: [] });
    const blocks = saved.blocks.sections[0]?.columns[0] ?? [];
    expect(blocks[0]?.["visible_if"]).toEqual({ any: [1] });
    expect(blocks[1]?.["src"]).toEqual({ asset: "0192b1a4-7c3e-7000-8000-00000000b001" });
    expect(blocks[2]).toEqual({ type: "snippet", id: "n", snippet_id: "0192b1a4-7c3e-7000-8000-00000000c001" });
  });

  it("shows an asset image read-only and edits the address of a url image", async () => {
    mockApi(templateRoutes({ detail: detailOf(rich()) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("group", { name: "Image block 2" }));
    expect(await screen.findByRole("textbox", { name: "Image source" })).toHaveAttribute("readonly");
    expect(screen.queryByRole("textbox", { name: "Image address" })).toBeNull();
  });

  it("starts a new document at schema 2 with the default theme and the theme footer", () => {
    const starting = startingDocument("email", "blocks", "marketing") as { schema: number; blocks: Doc };
    expect(starting.schema).toBe(2);
    expect(starting.blocks["theme"]).toBe("default");
    expect(starting.blocks["footer"]).toBe("theme");
  });

  it("reads a schema 1 document as one with no theme and no footer and an image url", () => {
    const old = { schema: 1, channel: "email", mode: "blocks", blocks: { subject: "S", sections: [{ id: "s", columns: [[{ type: "image", id: "i", src: "https://a.example/x.png", alt: "" }]]}] } };
    const read = readDocument(old);
    expect(read.kind === "blocks" && read.content.theme).toBe("none");
    expect(read.kind === "blocks" && read.content.footer).toBe("none");
    expect(read.kind === "blocks" && read.content.sections[0]?.columns[0]?.[0]).toMatchObject({ src: { url: "https://a.example/x.png" } });
    expect(read.kind === "blocks" && read.newer).toBe(false);
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
    mockApi(templateRoutes({ detail: detailOf(blocksDocument([], {}, 3)) }));
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

const THEME_WARM = "0192b1a4-7c3e-7000-8000-00000000d001";
const THEME_COOL = "0192b1a4-7c3e-7000-8000-00000000d002";
const SNIPPET = "0192b1a4-7c3e-7000-8000-00000000c001";
const THEMES = [
  listItem({ id: THEME_WARM, name: "Warm", current_version: 1, is_default: true }, 0, tokens({ footer_snippet: SNIPPET })),
  listItem({ id: THEME_COOL, name: "Cool", current_version: 2, is_default: false }),
];
const SNIPPETS = [{ ...snippetBody({ id: SNIPPET, name: "Shipping promise", current_version: 3 }), used_by_templates: 0 }];
const withThemes = (): Record<string, Handler> => ({ [`GET ${WS}/themes`]: () => json(THEMES) });
const withSnippets = (): Record<string, Handler> => ({ [`GET ${WS}/snippets`]: () => json(SNIPPETS) });
const savedBlocks = (calls: ReturnType<typeof mockApi>) =>
  (lastDraft(calls) as unknown as { blocks: Doc & { sections: { visible_if?: unknown; columns: Doc[][] }[] } }).blocks;

describe("the theme tab", () => {
  it("switches between the workspace default and a named theme and never offers none", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument([], {}, 2, { theme: "default", footer: "theme" })), extra: withThemes() }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Theme: Warm" }));
    const select = await screen.findByRole("combobox", { name: "Theme" });
    expect(within(select).getAllByRole("option").map((option) => option.textContent)).toEqual(["Follow the workspace default", "Warm", "Cool"]);
    expect(screen.getByRole("link", { name: "Edit theme" })).toHaveAttribute("href", `/w/ws1/themes/${THEME_WARM}`);

    await user.selectOptions(select, THEME_COOL);
    expect(await screen.findByRole("tab", { name: "Theme: Cool" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Edit theme" })).toHaveAttribute("href", `/w/ws1/themes/${THEME_COOL}`);
    await waitFor(() => expect(savedBlocks(calls)["theme"]).toEqual({ id: THEME_COOL }), WAIT);

    await user.selectOptions(screen.getByRole("combobox", { name: "Theme" }), "default");
    await waitFor(() => expect(savedBlocks(calls)["theme"]).toBe("default"), WAIT);
    expect(screen.queryByRole("tab", { name: "Document style" })).toBeNull();
  });

  it("shows Document style, a notice and Switch to a theme for a document at none, behind a confirm", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument([], {}, 2, { theme: "none", footer: "none" })), extra: withThemes() }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Document style" }));
    expect(screen.getByText(/has its own style and no theme/)).toBeInTheDocument();
    expect(screen.getByRole("spinbutton", { name: "Content width in pixels" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Switch to a theme" }));
    const dialog = await screen.findByRole("dialog", { name: "Switch to a theme?" });
    expect(within(dialog).getByText(/can only be undone from a version/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByRole("tab", { name: "Document style" })).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Switch to a theme" }));
    await user.click(within(await screen.findByRole("dialog", { name: "Switch to a theme?" })).getByRole("button", { name: "Switch to a theme" }));
    expect(await screen.findByRole("tab", { name: "Theme: Warm" })).toBeInTheDocument();
    await waitFor(() => expect(savedBlocks(calls)["theme"]).toBe("default"), WAIT);
  });

  it("offers the theme values as merge tags only while a theme is in effect", async () => {
    mockApi(templateRoutes({ detail: detailOf(blocksDocument([], {}, 2, { theme: "default" })), extra: withThemes() }));
    mount();
    expect(await screen.findByRole("heading", { name: "Theme" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Insert theme.primary" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Insert theme.logo_url" })).toBeInTheDocument();
  });

  it("offers no theme merge tags for a document with no theme", async () => {
    mockApi(templateRoutes({ detail: detailOf(blocksDocument([], {}, 2, { theme: "none", footer: "none" })), extra: withThemes() }));
    mount();
    expect(await screen.findByRole("button", { name: "Insert system.unsubscribe_url" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Insert theme.primary" })).toBeNull();
  });
});

describe("themes and snippets that are archived or not published", () => {
  const OLD = "0192b1a4-7c3e-7000-8000-00000000d003";
  const DRAFT = "0192b1a4-7c3e-7000-8000-00000000d004";
  const named = (id: string, extra: Doc = {}) =>
    detailOf(blocksDocument([{ id: "s", columns: [[{ type: "image", id: "i", src: { url: "" }, alt: "A bag" }]] }], {}, 2, { theme: { id }, footer: "theme", ...extra }));
  const themes = (): Record<string, Handler> => ({
    [`GET ${WS}/themes`]: () =>
      json([
        ...THEMES,
        listItem({ id: OLD, name: "Retired", current_version: 4, archived_at: "2026-10-20T09:00:00Z" }, 0, tokens({ primary: "#112233", footer_snippet: SNIPPET })),
        listItem({ id: DRAFT, name: "Unfinished", current_version: null }, 0, tokens({ footer_snippet: SNIPPET })),
      ]),
    [`GET ${WS}/assets`]: () => json(listBody([assetBody(1)])),
  });

  it("resolves an archived named theme by name, values and link, marks it archived, and keeps it in effect", async () => {
    mockApi(templateRoutes({ detail: named(OLD), extra: themes() }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Theme: Retired" }));
    expect(screen.getByText("Archived")).toBeInTheDocument();
    expect(screen.getByText("#112233")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Edit theme" })).toHaveAttribute("href", `/w/ws1/themes/${OLD}`);
    expect(within(screen.getByRole("combobox", { name: "Theme" })).queryByRole("option", { name: "Retired" })).toBeNull();
    expect(screen.getByRole("group", { name: "Theme footer" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Insert theme.primary" })).toBeInTheDocument();
  });

  it("treats a named theme with no published version as broken: no logo, no theme tags, no footer", async () => {
    mockApi(templateRoutes({ detail: named(DRAFT), extra: themes() }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("tab", { name: "Theme: Unfinished" }));
    expect(screen.getByText(/This theme is not published/)).toBeInTheDocument();
    expect(screen.queryByRole("group", { name: "Theme footer" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Insert theme.primary" })).toBeNull();
    await user.click(screen.getByRole("group", { name: "Image block 1" }));
    await user.click(await screen.findByRole("button", { name: "Choose image…" }));
    await screen.findByRole("radio", { name: /photo-1.png/ });
    expect(screen.queryByRole("radio", { name: /Theme logo/ })).toBeNull();
  });

  it("resolves an archived snippet by name and version with its link, marked archived", async () => {
    const archived = [{ ...snippetBody({ id: SNIPPET, name: "Old promise", current_version: 2, archived_at: "2026-10-20T09:00:00Z" }), used_by_templates: 1 }];
    mockApi(
      templateRoutes({
        detail: detailOf(blocksDocument([{ id: "s", columns: [[{ type: "snippet", id: "n", snippet_id: SNIPPET }]] }], {}, 2, { theme: "none", footer: "none" })),
        extra: { [`GET ${WS}/snippets`]: () => json(archived) },
      }),
    );
    mount();

    const card = await screen.findByRole("group", { name: "Snippet block 1" });
    expect(await within(card).findByText("Old promise")).toBeInTheDocument();
    expect(within(card).getByText(/version 2/)).toBeInTheDocument();
    expect(within(card).getByText(/Archived/)).toBeInTheDocument();
    expect(within(card).getByRole("link", { name: "Open snippet" })).toHaveAttribute("href", `/w/ws1/snippets/${SNIPPET}`);
  });
});

describe("the snippet block", () => {
  it("is inserted from a picker of published snippets, shows its name and version, and is removed", async () => {
    const calls = mockApi(templateRoutes({ detail: detailOf(blocksDocument([{ id: "s", columns: [[]] }])), extra: withSnippets() }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Add snippet block" }));
    await user.click(await screen.findByRole("button", { name: "Add snippet Shipping promise" }));

    const card = await screen.findByRole("group", { name: "Snippet block 1" });
    expect(within(card).getByText("Shipping promise")).toBeInTheDocument();
    expect(within(card).getByText(/version 3/)).toBeInTheDocument();
    expect(within(card).getByRole("link", { name: "Open snippet" })).toHaveAttribute("href", `/w/ws1/snippets/${SNIPPET}`);
    await waitFor(() => expect(savedBlocks(calls).sections[0]?.columns[0]).toEqual([{ type: "snippet", id: "snippet-1", snippet_id: SNIPPET }]), WAIT);

    await user.click(within(card).getByRole("button", { name: "Remove Snippet block 1" }));
    await waitFor(() => expect(savedBlocks(calls).sections[0]?.columns[0]).toEqual([]), WAIT);
  });

  it("says so when no snippet is published", async () => {
    mockApi(templateRoutes({ detail: detailOf(blocksDocument([{ id: "s", columns: [[]] }])) }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Add snippet block" }));
    expect(await screen.findByText(/No snippet is published yet/)).toBeInTheDocument();
  });
});

describe("the theme footer", () => {
  const footerDoc = (extra: Doc = {}) =>
    detailOf(blocksDocument([{ id: "s", columns: [[heading("h", "Hi")]] }], {}, 2, { theme: "default", footer: "theme", ...extra }));

  it("is the locked last item, removed and restored from its menu, with the unsubscribe notice when removed", async () => {
    const calls = mockApi(templateRoutes({ detail: footerDoc(), extra: withThemes() }));
    const user = userEvent.setup();
    mount();

    const footer = await screen.findByRole("group", { name: "Theme footer" });
    expect(within(footer).getByText("Theme footer · from Warm")).toBeInTheDocument();
    expect(within(footer).getByRole("img", { name: /Locked/ })).toBeInTheDocument();
    expect(within(footer).queryByRole("img", { name: /Drag handle/ })).toBeNull();
    expect(screen.getByRole("group", { name: "Canvas" }).lastElementChild).toBe(footer);

    await user.click(within(footer).getByRole("button", { name: "Theme footer actions" }));
    await user.click(screen.getByRole("menuitem", { name: "Remove" }));
    expect(await screen.findByText(/needs its own unsubscribe link/)).toBeInTheDocument();
    await waitFor(() => expect(savedBlocks(calls)["footer"]).toBe("none"), WAIT);

    await user.click(screen.getByRole("button", { name: "Theme footer actions" }));
    await user.click(screen.getByRole("menuitem", { name: "Restore" }));
    await waitFor(() => expect(savedBlocks(calls)["footer"]).toBe("theme"), WAIT);
    expect(screen.queryByText(/needs its own unsubscribe link/)).toBeNull();
  });

  it("shows no unsubscribe notice for a transactional message when removed", async () => {
    mockApi(templateRoutes({ detail: footerDoc({ category: "transactional", footer: "none" }), extra: withThemes() }));
    mount();
    expect(await screen.findByText(/removed from this email/)).toBeInTheDocument();
    expect(screen.queryByText(/needs its own unsubscribe link/)).toBeNull();
  });

  it("is not on the canvas when the theme names no footer snippet", async () => {
    const bare = [listItem({ id: THEME_WARM, name: "Warm", current_version: 1, is_default: true })];
    mockApi(templateRoutes({ detail: footerDoc(), extra: { [`GET ${WS}/themes`]: () => json(bare) } }));
    mount();
    await screen.findByRole("tab", { name: "Theme: Warm" });
    expect(screen.queryByRole("group", { name: "Theme footer" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Theme footer actions" })).toBeNull();
  });

  it("is not on the canvas for a document with no theme", async () => {
    mockApi(templateRoutes({ detail: footerDoc({ theme: "none", footer: "none" }), extra: withThemes() }));
    mount();
    await screen.findByRole("group", { name: "Heading block 1" });
    expect(screen.queryByRole("group", { name: "Theme footer" })).toBeNull();
  });
});

describe("the image source", () => {
  const withImage = (extra: Doc = {}) =>
    detailOf(blocksDocument([{ id: "s", columns: [[{ type: "image", id: "i", src: { url: "" }, alt: "A bag" }]] }], {}, 2, { theme: "default", ...extra }));
  const library = (): Record<string, Handler> => ({ ...withThemes(), [`GET ${WS}/assets`]: () => json(listBody([assetBody(1)])) });
  const sourceOf = (calls: ReturnType<typeof mockApi>) => savedBlocks(calls).sections[0]?.columns[0]?.[0]?.["src"];

  it("sets an asset from the library", async () => {
    const calls = mockApi(templateRoutes({ detail: withImage(), extra: library() }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("group", { name: "Image block 1" }));
    await user.click(await screen.findByRole("button", { name: "Choose image…" }));
    await user.click(await screen.findByRole("radio", { name: /photo-1\.png/ }));
    await user.click(screen.getByRole("button", { name: "Use image" }));

    await waitFor(() => expect(sourceOf(calls)).toEqual({ asset: assetBody(1).id }), WAIT);
    expect(screen.getByRole("textbox", { name: "Image source" })).toHaveValue(`Uploaded image ${assetBody(1).id}`);
  });

  it("sets the https address as a url", async () => {
    const calls = mockApi(templateRoutes({ detail: withImage(), extra: library() }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("group", { name: "Image block 1" }));
    await user.click(await screen.findByRole("button", { name: "Choose image…" }));
    await user.click(await screen.findByRole("button", { name: "Use a web address" }));
    await user.click(screen.getByRole("textbox", { name: "Web address" }));
    await user.paste("https://cdn.example/bag.png");
    await user.click(screen.getByRole("button", { name: "Use image" }));

    await waitFor(() => expect(sourceOf(calls)).toEqual({ url: "https://cdn.example/bag.png" }), WAIT);
  });

  it("offers the theme logo while a theme is in effect, and sets it", async () => {
    const calls = mockApi(templateRoutes({ detail: withImage(), extra: library() }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("group", { name: "Image block 1" }));
    await screen.findByRole("tab", { name: "Theme: Warm" });
    await user.click(screen.getByRole("button", { name: "Choose image…" }));
    await user.click(await screen.findByRole("radio", { name: /Theme logo/ }));
    await user.click(screen.getByRole("button", { name: "Use image" }));

    await waitFor(() => expect(sourceOf(calls)).toEqual({ theme: "logo" }), WAIT);
    expect(screen.getByRole("textbox", { name: "Image source" })).toHaveValue("The theme logo");
  });

  it("offers no theme logo for a document with no theme", async () => {
    mockApi(templateRoutes({ detail: withImage({ theme: "none", footer: "none" }), extra: library() }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("group", { name: "Image block 1" }));
    await user.click(await screen.findByRole("button", { name: "Choose image…" }));
    await screen.findByRole("radio", { name: /photo-1\.png/ });
    expect(screen.queryByRole("radio", { name: /Theme logo/ })).toBeNull();
  });
});

describe("visibility", () => {
  const withHeading = (visible?: unknown) =>
    detailOf(
      blocksDocument([{ id: "s", columns: [[{ ...heading("h", "Hi"), ...(visible ? { visible_if: visible } : {}) }]] }], {}, 2, { theme: "none", footer: "none" }),
    );

  it("writes visible_if from the Show row and removes it again", async () => {
    const calls = mockApi(templateRoutes({ detail: withHeading() }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("group", { name: "Heading block 1" }));
    const row = await screen.findByRole("group", { name: "Visibility" });
    expect(within(row).getByRole("button", { name: "Always" })).toHaveAttribute("aria-pressed", "true");
    await user.click(within(row).getByRole("button", { name: "Only when" }));

    await waitFor(() => expect(savedBlocks(calls).sections[0]?.columns[0]?.[0]?.["visible_if"]).toEqual({ all: [] }), WAIT);
    expect(within(screen.getByRole("group", { name: "Heading block 1" })).getByText("Only when…")).toBeInTheDocument();

    await user.click(within(row).getByRole("button", { name: "Always" }));
    await waitFor(() => expect(savedBlocks(calls).sections[0]?.columns[0]?.[0]).not.toHaveProperty("visible_if"), WAIT);
  });

  it("shows a notice instead of crashing for a condition the editor cannot show, and replaces it", async () => {
    const calls = mockApi(
      templateRoutes({
        detail: withHeading({ any: [1] }),
        preview: () => previewBody({ issues: [blocking("invalid_condition", "/blocks/block/h/visible_if")] }),
      }),
    );
    const user = userEvent.setup();
    mount();

    const block = await screen.findByRole("group", { name: "Heading block 1" });
    expect(within(block).getByText("Only when…")).toBeInTheDocument();
    await user.click(block);
    const row = await screen.findByRole("group", { name: "Visibility" });
    expect(within(row).getByText("This condition can't be shown here.")).toBeInTheDocument();
    expect(within(row).getByText(/Blocks publishing/)).toBeInTheDocument();
    await user.click(within(row).getByRole("button", { name: "Replace with a new condition" }));
    await waitFor(() => expect(savedBlocks(calls).sections[0]?.columns[0]?.[0]?.["visible_if"]).toEqual({ all: [] }), WAIT);
    expect(within(screen.getByRole("group", { name: "Visibility" })).queryByText("This condition can't be shown here.")).toBeNull();
  });

  it("treats a null visible_if as no condition", async () => {
    const stored = blocksDocument([{ id: "s", visible_if: null, columns: [[{ ...heading("h", "Hi"), visible_if: null }]] }], {}, 2, { theme: "none", footer: "none" });
    mockApi(templateRoutes({ detail: detailOf(stored) }));
    mount();

    const block = await screen.findByRole("group", { name: "Heading block 1" });
    expect(within(block).queryByText("Only when…")).toBeNull();
    expect(within(screen.getByRole("group", { name: "Section 1, 1 column" })).queryByText("Only when…")).toBeNull();
  });

  it("is offered on a section as well", async () => {
    const calls = mockApi(templateRoutes({ detail: withHeading() }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Section 1 settings" }));
    await user.click(within(await screen.findByRole("group", { name: "Visibility" })).getByRole("button", { name: "Only when" }));
    await waitFor(() => expect(savedBlocks(calls).sections[0]?.visible_if).toEqual({ all: [] }), WAIT);
  });

  it("shows an issue pointed at a block's condition on that block and in its Visibility row", async () => {
    mockApi(
      templateRoutes({
        detail: withHeading({ all: [] }),
        preview: () => previewBody({ issues: [blocking("invalid_condition", "/blocks/block/h/visible_if")] }),
      }),
    );
    const user = userEvent.setup();
    mount();

    const block = await screen.findByRole("group", { name: "Heading block 1" });
    expect(await within(block).findByText(/Blocks publishing/)).toBeInTheDocument();
    await user.click(block);
    expect(within(await screen.findByRole("group", { name: "Visibility" })).getByText(/Blocks publishing/)).toBeInTheDocument();
  });

  it("shows an issue pointed at a section's condition on that section", async () => {
    mockApi(
      templateRoutes({
        detail: withHeading(),
        preview: () => previewBody({ issues: [blocking("invalid_condition", "/blocks/sections/s/visible_if")] }),
      }),
    );
    mount();

    const section = await screen.findByRole("group", { name: "Section 1, 1 column" });
    expect(await within(section).findByText(/Blocks publishing/)).toBeInTheDocument();
  });
});

describe("hidden blocks in the preview", () => {
  const doc = () => detailOf(blocksDocument([{ id: "s", columns: [[heading("h", "Hi"), heading("g", "Bye")]] }], {}, 2, { theme: "none", footer: "none" }));

  it("says how many blocks are hidden and why, and goes to the block", async () => {
    Element.prototype.scrollIntoView = vi.fn();
    mockApi(templateRoutes({ detail: doc(), preview: () => previewBody({ hidden_blocks: [{ id: "g", reason: "not in Coffee club" }] }) }));
    const user = userEvent.setup();
    mount();

    const line = await screen.findByText(/block hidden for the sample person/);
    expect(line).toHaveTextContent("1 block hidden for the sample person: not in Coffee club.");
    await user.click(within(line).getByRole("button", { name: "Go to block" }));

    expect(await screen.findByRole("textbox", { name: "Heading text" })).toHaveValue("Bye");
    expect(Element.prototype.scrollIntoView).toHaveBeenCalled();
    expect(screen.getByRole("group", { name: "Heading block 2" })).toHaveFocus();
  });

  it("shows a line only when something is hidden", async () => {
    mockApi(templateRoutes({ detail: doc() }));
    mount();
    await screen.findByTitle("Preview of the message");
    expect(screen.queryByText(/hidden for/)).toBeNull();
  });
});

describe("schema 2 documents on the wire", () => {
  it("saves a document of a newer schema back byte-identical", () => {
    const stored = {
      schema: 3,
      channel: "email",
      mode: "blocks",
      blocks: { subject: "S", future: { a: [1, 2] }, sections: [{ id: "s", columns: [[{ type: "hologram", id: "x" }]] }] },
    };
    const read = readDocument(stored);
    expect(read.kind === "blocks" && read.newer).toBe(true);
    expect(JSON.stringify(writeDocument(read))).toBe(JSON.stringify(stored));
  });

  it("carries fields it does not know on the document, a section and a block through a read and a write", () => {
    const stored = {
      schema: 2,
      channel: "email",
      mode: "blocks",
      blocks: {
        subject: "S",
        preheader: "",
        category: "marketing",
        theme: "default",
        footer: "theme",
        extra_top: 1,
        settings: { ...DEFAULT_STYLE, extra_setting: true },
        sections: [{ id: "s", extra_section: "x", columns: [[{ type: "heading", id: "h", level: 2, text: "Hi", extra_block: [1] }]] }],
      },
    };
    const written = writeDocument(readDocument(stored)) as { blocks: Doc };
    expect(written.blocks).toEqual(stored.blocks);
  });
});
