import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { expect, test, type APIRequestContext, type Page } from "@playwright/test";

import { signIn } from "./support";
import { asLines, normaliseTracking, unifiedDiff } from "./tracking";

const LOGO = resolve(import.meta.dirname, "fixtures/logo.png");
const REFERENCE = resolve(import.meta.dirname, "../../crates/zirv-flow-templates/tests/fixtures/blocks/newsletter.doc.json");
const ADDRESS_LINE = "Spring Street 1, 1000 Copenhagen";
/** The public URL of the test server, and the address the button links to. */
const FORMS = { publicUrl: "https://flow.browser-test.example", direct: ["https://shop.example/spring"] };

interface Captured {
  html: string;
  text: string;
}

/** The block type each canvas label stands for. */
const TYPES: Record<string, string> = {
  Heading: "heading",
  Text: "text",
  Image: "image",
  Button: "button",
  Divider: "divider",
  Spacer: "spacer",
  "Raw HTML": "raw_html",
  Snippet: "snippet",
};

async function openLibrary(page: Page, tab: "Templates" | "Themes" | "Snippets" | "Images"): Promise<void> {
  const nav = page.getByRole("navigation", { name: "Template library" });
  if (!(await nav.isVisible())) await page.getByRole("link", { name: "Templates" }).first().click();
  await nav.getByRole("link", { name: tab }).click();
}

async function savedDraft(page: Page): Promise<void> {
  await expect(page.getByText("Draft saved")).toBeVisible();
}

async function uploadLogo(page: Page): Promise<void> {
  await openLibrary(page, "Images");
  await page.getByLabel("Choose an image file").setInputFiles(LOGO);
  await expect(page.getByRole("button", { name: /logo\.png/ })).toBeVisible();
}

async function publishSnippet(page: Page): Promise<void> {
  await openLibrary(page, "Snippets");
  await page.getByRole("button", { name: "New snippet" }).first().click();
  await page.getByLabel("Name").fill("Legal footer");
  await page.getByRole("button", { name: "Create snippet" }).click();

  await page.getByRole("button", { name: "Add text block" }).click();
  await page.getByRole("textbox", { name: "Text", exact: true }).fill(ADDRESS_LINE);
  await page.getByRole("button", { name: "Add text block" }).click();
  const unsubscribe = page.getByRole("textbox", { name: "Text", exact: true });
  await unsubscribe.fill("Unsubscribe");
  await unsubscribe.press("Control+A");
  await page.getByRole("button", { name: "Add a link" }).click();
  await page.getByLabel("Link address").fill("{{ system.unsubscribe_url }}");
  await page.getByRole("button", { name: "Apply link" }).click();

  await savedDraft(page);
  await page.getByRole("button", { name: "Publish…" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Publish", exact: true }).click();
  await expect(page.getByText(/Published version 1/)).toBeVisible();
}

async function publishDefaultTheme(page: Page): Promise<void> {
  await openLibrary(page, "Themes");
  await page.getByRole("button", { name: "New theme" }).first().click();
  await page.getByLabel("Name").fill("Spring");
  await page.getByRole("button", { name: "Create theme" }).click();

  await page.getByLabel("Primary").fill("#0b7a4b");
  await page.getByLabel("Button", { exact: true }).fill("#0b7a4b");
  await page.getByLabel("Font").selectOption({ index: 1 });
  await page.getByRole("button", { name: "Change" }).click();
  await page.getByRole("radio", { name: /logo\.png/ }).click();
  await page.getByRole("button", { name: "Use image" }).click();
  await page.getByLabel("Footer snippet").selectOption({ label: "Legal footer" });

  await savedDraft(page);
  await page.getByRole("button", { name: "Publish…" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Publish", exact: true }).click();
  await expect(page.getByText(/Published version 1/)).toBeVisible();

  await page.getByRole("button", { name: "More actions for Spring" }).click();
  await page.getByRole("menuitem", { name: "Set as default" }).click();
  await page.getByRole("button", { name: "Make default" }).click();
  await expect(page.getByText("Default", { exact: true })).toBeVisible();
}

async function addBlock(page: Page, type: string): Promise<void> {
  await page.getByRole("button", { name: `Add ${type} block` }).click();
}

async function buildLayout(page: Page): Promise<void> {
  await openLibrary(page, "Templates");
  await page.getByRole("button", { name: "New template" }).first().click();
  await page.getByLabel("Name").fill("Spring newsletter");
  await page.getByLabel(/^Blocks/).check();
  await page.getByRole("button", { name: "Create template" }).click();

  const section = (columns: number) => page.getByRole("button", { name: `Add a section with ${columns} ${columns === 1 ? "column" : "columns"}` }).click();
  const text = (content: string) => page.getByRole("textbox", { name: "Text", exact: true }).fill(content);

  await section(1);
  await addBlock(page, "image");
  await page.getByRole("button", { name: "Choose image…" }).click();
  await page.getByRole("radio", { name: /logo\.png/ }).click();
  await page.getByRole("button", { name: "Use image" }).click();
  await page.getByLabel("Alt text").fill("Spring collection");

  await section(1);
  await addBlock(page, "heading");
  await page.getByLabel("Heading text").fill("Hello there");
  await addBlock(page, "text");
  await text("Our spring collection is here.");

  await section(2);
  await addBlock(page, "heading");
  await page.getByLabel("Heading text").fill("Fresh");
  await addBlock(page, "text");
  await text("New arrivals every week.");
  await addBlock(page, "heading");
  await page.getByLabel("Heading text").fill("Fair");
  await addBlock(page, "text");
  await text("Prices that stay put.");
  // A block at the end of a column moves down into the next column.
  await page.getByRole("button", { name: "Move Text block 7 down" }).click();
  await page.getByRole("button", { name: "Move Heading block 6 down" }).click();

  await section(1);
  await addBlock(page, "button");
  await page.getByRole("textbox", { name: "Label", exact: true }).fill("See the collection");
  await page.getByRole("textbox", { name: "URL", exact: true }).fill("https://shop.example/spring");
  await addBlock(page, "divider");

  await section(1);
  await page.getByRole("button", { name: "Add snippet block" }).click();
  await page.getByRole("button", { name: "Add snippet Legal footer" }).click();

  await page.getByRole("tab", { name: "Message" }).click();
  await page.getByLabel("Subject").fill("Spring is here");
  await page.getByLabel("Preheader").fill("A preview of the collection");
  await page.getByLabel("Sender identity").selectOption({ index: 1 });
  await savedDraft(page);
}

/** The block types of the saved draft in canvas order, read after a reload so they come from the server. */
async function savedBlockTypes(page: Page): Promise<string[]> {
  await page.reload();
  const canvas = page.getByRole("group", { name: "Canvas" });
  await expect(canvas).toBeVisible();
  const labels = await canvas.locator("[data-block-id]").evaluateAll((blocks) => blocks.map((block) => block.getAttribute("aria-label") ?? ""));
  return labels.map((label) => {
    const type = TYPES[label.replace(/ block \d+$/, "")];
    if (!type) throw new Error(`an unknown block on the canvas: ${label}`);
    return type;
  });
}

/** The reference document's block types in reading order, with the footer text block as the snippet it is built from here. */
function referenceTypes(): string[] {
  const document = JSON.parse(readFileSync(REFERENCE, "utf8")) as {
    blocks: { sections: { columns: { type: string; id: string }[][] }[] };
  };
  return document.blocks.sections
    .flatMap((section) => section.columns.flat())
    .map((block) => (block.id === "footer_text" ? "snippet" : block.type));
}


/** The HTML and text parts of what the preview pane shows for the sample person. */
async function previewOf(page: Page): Promise<Captured> {
  const frame = page.locator('iframe[title="Preview of the message"]');
  await expect(frame).toHaveAttribute("srcdoc", /Prices that stay put/);
  const html = await frame.getAttribute("srcdoc");
  await page.getByRole("button", { name: "Text", exact: true }).click();
  const text = await page.getByLabel("Text part").textContent();
  await page.getByRole("button", { name: "HTML", exact: true }).click();
  if (html === null || text === null) throw new Error("the preview shows no message");
  return { html: asLines(html), text: asLines(text) };
}

/** Test-sends the published template to the allowlisted address, and returns the message the sink got. */
async function sendTest(page: Page, request: APIRequestContext, already: number): Promise<Captured> {
  await page.getByRole("button", { name: "Send a test" }).click();
  await expect(page.getByRole("heading", { name: "Send a test", level: 1 })).toBeVisible();
  await page.getByRole("group", { name: "Test inboxes" }).getByRole("checkbox").first().check();
  await page.getByRole("button", { name: /^Version \d+ · live$/ }).click();
  await page.getByLabel("From").selectOption({ index: 1 });
  await page.getByRole("button", { name: "Send 1 test" }).click();
  await expect(page.getByRole("status", { name: "Results" })).toContainText("Queued");

  const sink = async () => (await (await request.get("/__test/sink")).json()) as { html: string; text: string }[];
  await expect.poll(async () => (await sink()).length).toBe(already + 1);
  const message = (await sink())[already];
  if (!message) throw new Error("the sink holds no message");
  return { html: asLines(message.html), text: asLines(message.text) };
}

/** The unified diffs of the sent message against the preview, after the tracking forms are mapped. */
function differences(preview: Captured, sent: Captured): { html: string; text: string } {
  const same = (part: string) => normaliseTracking(part, FORMS);
  return { html: unifiedDiff(same(preview.html), same(sent.html)), text: unifiedDiff(same(preview.text), same(sent.text)) };
}

/** Changes the theme's button colour and publishes the result as the next version. */
async function recolourTheme(page: Page): Promise<void> {
  await openLibrary(page, "Themes");
  await page.getByRole("link", { name: "Spring" }).click();
  await page.getByLabel("Button", { exact: true }).fill("#c2185b");
  await savedDraft(page);
  await page.getByRole("button", { name: "Publish…" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Publish", exact: true }).click();
  await expect(page.getByText(/Published version 2/)).toBeVisible();
}

test("the newsletter built in the browser is sent as the preview shows it", async ({ page, request }) => {
  await signIn(page);
  await page.getByRole("link", { name: "Templates" }).first().click();

  await uploadLogo(page);
  await publishSnippet(page);
  await publishDefaultTheme(page);
  await buildLayout(page);

  expect(await savedBlockTypes(page)).toEqual(referenceTypes());

  await page.getByRole("button", { name: "Publish…" }).click();
  await page.getByRole("dialog").getByRole("button", { name: /^Publish version 1$/ }).click();
  await expect(page.getByRole("dialog")).toBeHidden();

  const preview = await previewOf(page);
  const sent = await sendTest(page, request, 0);

  const same = differences(preview, sent);
  expect(same.html, "the sent HTML against the preview").toBe("");
  expect(same.text, "the sent text against the preview").toBe("");

  // The control: with one theme colour changed after the preview was taken, the same comparison fails.
  await recolourTheme(page);
  await openLibrary(page, "Templates");
  await page.getByRole("link", { name: "Spring newsletter" }).click();
  const recoloured = await sendTest(page, request, 1);

  const changed = differences(preview, recoloured);
  expect(changed.html, "the comparison must see a changed theme colour").toContain("#c2185b");
  expect(changed.text).toBe("");
});
