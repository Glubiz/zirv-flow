import { randomBytes } from "node:crypto";
import { appendFileSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

import { expect, test, type Page } from "@playwright/test";

import { openPage } from "../support";

const RECIPIENT = "reader@example.com";
const NEWSLETTER = "Spring newsletter";
const ADDRESS = "Spring Street 1, 1000 Copenhagen";
const SCREENSHOTS = resolve(import.meta.dirname, "../test-results/guide-screenshots");

interface Message {
  subject: string;
  recipients: number;
  html: string;
  text: string;
}

function scratch(name: string): string {
  const directory = process.env.DRILL_WORK;
  if (!directory || process.env.CI !== "true") throw new Error("Run via scripts/drill/guide.sh in CI.");
  return resolve(directory, name);
}

function captured(): Message[] {
  return JSON.parse(readFileSync(scratch("messages.json"), "utf8")) as Message[];
}

async function nav(page: Page, name: string): Promise<void> {
  const settings = ["API keys", "Sending domains", "Destinations", "Sending limits"].includes(name);
  await openPage(page, name, settings ? "Settings" : "Main");
}

async function receipt(page: Page, index: number, subject: string): Promise<Message> {
  await expect.poll(() => captured().length, { timeout: 90_000 }).toBe(index + 1);
  const message = captured()[index];
  if (!message) throw new Error("The SMTP sink has no captured message.");
  expect(message.subject).toBe(subject);
  expect(message.recipients).toBe(1);
  for (const part of [message.html, message.text]) {
    expect(part).toContain("Our spring collection is here.");
    expect(part).toContain(ADDRESS);
    expect(part).toContain("Unsubscribe");
  }
  const unsubscribe = message.text.match(/https:\/\/localhost:8443\/u\/[A-Za-z0-9_-]+/)?.[0];
  if (!unsubscribe) throw new Error("The text part has no unsubscribe URL on the configured public base.");
  const links = await page.evaluate((html) => {
    const parsed = new DOMParser().parseFromString(html, "text/html");
    return Array.from(parsed.querySelectorAll("a[href]"), (anchor) => anchor.getAttribute("href"));
  }, message.html);
  expect(links).toContain(unsubscribe);
  return message;
}

test("getting started on a clean Compose stack", async ({ page, request }) => {
  let profileUrl = "";
  let templateUrl = "";

  await test.step("1. Claim the instance through setup", async () => {
    const token = readFileSync(scratch("setup-token"), "utf8").trim();
    const password = randomBytes(24).toString("hex");
    console.log(`::add-mask::${token}`);
    console.log(`::add-mask::${password}`);
    await page.goto("/setup");
    await page.getByLabel("Setup token").fill(token);
    await page.getByLabel("Owner email").fill("owner@example.com");
    await page.getByLabel("Password").fill(password);
    await page.getByRole("button", { name: "Create owner and sign in" }).click();
    await expect(page.getByRole("heading", { name: "Workspaces", exact: true })).toBeVisible();
  });

  await test.step("2. Create and select the workspace", async () => {
    await page.getByRole("button", { name: "New workspace" }).click();
    await page.getByRole("dialog").getByLabel("Name").fill("Newsletter guide");
    await page.getByRole("button", { name: "Create workspace" }).click();
    await expect(page.getByRole("dialog")).toBeHidden();
    await expect(page.getByRole("link", { name: "Open Newsletter guide" })).toBeVisible();
    await page.getByRole("button", { name: "Switch workspace" }).click();
    await page.getByRole("menuitemradio", { name: "Newsletter guide" }).click();
    await expect(page.getByRole("heading", { name: "Home", exact: true })).toBeVisible();
  });

  await test.step("3. Publish the schema, create an ingest key and watch the event arrive", async () => {
    await nav(page, "Events");
    await page.getByRole("button", { name: "Add schema" }).first().click();
    await page.getByLabel("Event name").fill("order_placed");
    await page.getByRole("dialog").getByRole("button", { name: "Add schema" }).click();
    await page.getByRole("link", { name: "order_placed", exact: true }).click();
    await page.getByRole("button", { name: "Publish", exact: true }).click();
    await page.getByRole("button", { name: "Publish version", exact: true }).click();
    await expect(page.getByRole("dialog")).toBeHidden();

    await nav(page, "API keys");
    await page.getByRole("button", { name: "Create key" }).first().click();
    const dialog = page.getByRole("dialog", { name: "New API key" });
    await dialog.getByLabel("Name").fill("Guide ingestion");
    await dialog.getByRole("checkbox", { name: /^Read / }).uncheck();
    await dialog.getByRole("checkbox", { name: /^Ingest / }).check();
    await dialog.getByRole("button", { name: "Create key" }).click();
    const key = await page.getByLabel("Secret for Guide ingestion", { exact: true }).textContent();
    if (!key) throw new Error("The UI did not display the ingestion key.");
    console.log(`::add-mask::${key.trim()}`);
    await page.getByRole("button", { name: "I have copied it" }).click();

    await nav(page, "Events");
    await page.getByRole("link", { name: "Debugger", exact: true }).click();
    await expect(page.getByRole("group", { name: "Feed controls" })).toContainText("Live");
    // The guide's curl step is the only application write made outside the UI.
    const response = await request.post("/api/v1/ingest/track", {
      headers: { Authorization: `Bearer ${key.trim()}` },
      data: { identifiers: { email: RECIPIENT }, event: "order_placed", properties: {}, messageId: "guide-order-1" },
    });
    expect(response.status()).toBe(202);
    await expect(page.getByRole("table", { name: "Live events" }).getByRole("row").filter({ hasText: "order_placed" })).toContainText("Accepted");
    await nav(page, "People");
    await page.getByLabel("Value", { exact: true }).fill(RECIPIENT);
    await page.getByRole("button", { name: "Search", exact: true }).click();
    await page.getByRole("table", { name: "People that match, ordered by person" }).getByRole("link", { name: /^Open / }).click();
    await expect(page.getByRole("list", { name: "Timeline, newest first" })).toContainText("order_placed");
    profileUrl = page.url();
  });

  await test.step("4. Preview and compute the segment, then record consent", async () => {
    await nav(page, "Segments");
    await page.getByRole("button", { name: "New segment" }).first().click();
    await page.getByLabel("Name", { exact: true }).fill("Newsletter readers");
    await page.getByLabel("Type", { exact: true }).selectOption({ label: "Event" });
    await page.getByLabel("Event", { exact: true }).selectOption({ label: "order_placed" });
    await expect(page.getByLabel("Time window")).toHaveValue("");
    await expect(page.getByText("1 person matches", { exact: true })).toBeVisible();
    await expect(page.getByRole("region", { name: "Sample of people" }).getByRole("link", { name: RECIPIENT })).toBeVisible();
    await page.getByRole("button", { name: "Create segment", exact: true }).click();
    await expect(page.getByRole("heading", { name: "Newsletter readers", exact: true })).toBeVisible();
    // The saved page does not poll membership; reload while the initial compute runs.
    await expect(async () => {
      await page.reload();
      await page.getByRole("navigation", { name: "Segment sections" }).getByRole("link", { name: "People", exact: true }).click();
      await expect(page.getByRole("table", { name: "People in this segment" }).getByRole("link", { name: RECIPIENT })).toBeVisible({ timeout: 2000 });
    }).toPass({ timeout: 90_000, intervals: [1000, 2000] });
    await page.goto(profileUrl);
    await page.getByLabel("Applies to").selectOption({ label: "All email" });
    await page.getByLabel("Choice", { exact: true }).selectOption({ label: "Subscribed" });
    await page.getByRole("button", { name: "Record choice" }).click();
    await expect(page.getByText("Recorded: subscribed.", { exact: true })).toBeVisible();
  });

  await test.step("5. Publish DNS, add the sender, TLS SMTP destination and test inbox", async () => {
    await nav(page, "Sending domains");
    await page.getByRole("button", { name: "Add a domain" }).first().click();
    await page.getByLabel("Domain", { exact: true }).fill("guide.example.com");
    await page.getByRole("dialog").getByRole("button", { name: "Add the domain" }).click();
    const records: Record<string, string> = {};
    for (const kind of ["SPF", "DKIM", "DMARC"]) {
      const record = page.getByRole("region", { name: `${kind} record`, exact: true });
      // RecordStep presents Host and Publish as adjacent label/value spans.
      const host = await record.locator(".rec-label").filter({ hasText: /^Host$/ }).locator("+ .rec-value").innerText();
      const value = await record.locator(".rec-label").filter({ hasText: /^Publish$/ }).locator("+ .rec-value").innerText();
      records[host] = value;
    }
    writeFileSync(scratch("records.new"), JSON.stringify(records));
    renameSync(scratch("records.new"), scratch("records.json"));
    await page.getByRole("button", { name: "Check DNS now" }).click();
    await expect(page.getByText("Verified", { exact: true })).toBeVisible({ timeout: 90_000 });
    await page.getByRole("button", { name: "Add sender identity" }).first().click();
    await page.getByLabel("From name").fill("Spring newsletter");
    await page.getByLabel("From address").fill("newsletter@guide.example.com");
    await page.getByRole("button", { name: "Add the identity" }).click();
    await expect(page.getByText("Can send", { exact: true })).toBeVisible();

    await nav(page, "Destinations");
    await page.getByRole("link", { name: "New destination" }).first().click();
    await page.getByLabel("Type", { exact: true }).selectOption({ label: "SMTP" });
    await page.getByLabel("Name", { exact: true }).fill("Newsletter capture");
    await page.getByLabel("CA certificate (optional)", { exact: true }).fill(readFileSync(scratch("ca.pem"), "utf8"));
    await page.getByRole("button", { name: "New credential" }).click();
    const credential = page.getByRole("dialog", { name: "New credential for SMTP" });
    await credential.getByLabel("Host", { exact: true }).fill("fixture");
    await credential.getByLabel("Port", { exact: true }).fill("2525");
    await credential.getByLabel("TLS mode", { exact: true }).fill("tls");
    await credential.getByLabel("Username", { exact: true }).fill("drill");
    await credential.getByLabel("Password", { exact: true }).fill("drill");
    await credential.getByRole("button", { name: "Create credential" }).click();
    await expect(credential).toBeHidden();
    await page.getByRole("button", { name: "Create destination" }).click();
    await expect(page.getByRole("heading", { name: "Newsletter capture", exact: true })).toBeVisible();
    await nav(page, "Sending limits");
    await page.getByLabel("Address", { exact: true }).fill(RECIPIENT);
    await page.getByRole("button", { name: "Add address" }).click();
    await expect(page.getByRole("list", { name: "Test inboxes", exact: true })).toContainText("r***@example.com");
  });

  const testMessage = await test.step("6. Build, preview, publish and test-send the newsletter", async () => {
    await nav(page, "Templates");
    await page.getByRole("button", { name: "New template" }).first().click();
    await page.getByLabel("Name", { exact: true }).fill(NEWSLETTER);
    await page.getByLabel(/^Blocks/).check();
    await page.getByRole("button", { name: "Create template" }).click();
    await page.getByRole("button", { name: "Add a section with 1 column" }).click();
    await page.getByRole("button", { name: "Add heading block" }).click();
    await page.getByLabel("Heading text").fill("Hello there");
    await page.getByRole("button", { name: "Add text block" }).click();
    await page.getByRole("textbox", { name: "Text", exact: true }).fill("Our spring collection is here.");
    await page.getByRole("button", { name: "Add button block" }).click();
    await page.getByRole("textbox", { name: "Label", exact: true }).fill("See the collection");
    await page.getByRole("textbox", { name: "URL", exact: true }).fill("https://shop.example/spring");
    await page.getByRole("button", { name: "Add text block" }).click();
    const footer = page.getByRole("textbox", { name: "Text", exact: true });
    await footer.fill(`${ADDRESS}\nUnsubscribe`);
    await footer.press("Control+End");
    await footer.press("Control+Shift+ArrowLeft");
    await page.getByRole("button", { name: "Add a link" }).click();
    await page.getByLabel("Link address").fill("{{ system.unsubscribe_url }}");
    await page.getByRole("button", { name: "Apply link" }).click();
    await page.getByRole("tab", { name: "Message", exact: true }).click();
    await page.getByLabel("Subject", { exact: true }).fill("Spring is here");
    await page.getByLabel("Preheader", { exact: true }).fill("A preview of the collection");
    await page.getByLabel("Sender identity", { exact: true }).selectOption({ index: 1 });
    await expect(page.getByText("Draft saved", { exact: true })).toBeVisible();
    templateUrl = page.url();
    const preview = page.getByRole("region", { name: "Preview", exact: true });
    await expect(preview.getByText("Spring is here", { exact: true })).toBeVisible();
    await expect(preview).not.toHaveAttribute("aria-busy", "true");
    await expect(preview.locator('iframe[title="Preview of the message"]')).toHaveAttribute("srcdoc", /Our spring collection is here/);
    await preview.getByRole("button", { name: "Text", exact: true }).click();
    await expect(preview.getByLabel("Text part")).toContainText(ADDRESS);
    await expect(preview.getByLabel("Text part")).toContainText("Unsubscribe");
    await preview.getByRole("button", { name: "HTML", exact: true }).click();
    mkdirSync(SCREENSHOTS, { recursive: true });
    await page.evaluate(() => document.fonts.ready.then(() => undefined));
    await page.getByRole("heading", { name: NEWSLETTER, exact: true }).scrollIntoViewIfNeeded();
    await page.screenshot({ path: resolve(SCREENSHOTS, "newsletter-editor.png"), animations: "disabled" });
    await preview.screenshot({ path: resolve(SCREENSHOTS, "newsletter-preview.png"), animations: "disabled" });
    await page.getByRole("button", { name: "Publish…", exact: true }).click();
    await expect(page.getByRole("heading", { name: "Review and publish version 1", exact: true })).toBeVisible();
    await page.getByRole("button", { name: "Publish version 1", exact: true }).first().click();
    await expect(page.getByRole("heading", { name: "Version 1 is live", exact: true, level: 1 })).toBeVisible();
    await page.goto(templateUrl);
    await page.getByRole("button", { name: "Send a test", exact: true }).click();
    await page.getByRole("group", { name: "Test inboxes" }).getByRole("checkbox", { name: "r***@example.com", exact: true }).check();
    await page.getByRole("button", { name: "Version 1 · live", exact: true }).click();
    await page.getByLabel("From", { exact: true }).selectOption({ index: 1 });
    await page.getByLabel("Send through", { exact: true }).selectOption({ label: "Newsletter capture" });
    expect(captured()).toHaveLength(0);
    await page.getByRole("button", { name: "Send 1 test", exact: true }).click();
    await expect(page.getByRole("status", { name: "Results" })).toContainText("Queued");
    return receipt(page, 0, "[TEST] Spring is here");
  });

  await test.step("7. Build and dry-run the flow without sending", async () => {
    await nav(page, "Flows");
    await page.getByRole("button", { name: "New flow" }).first().click();
    await page.getByLabel("Name", { exact: true }).fill(NEWSLETTER);
    await page.getByLabel("Starts when").selectOption({ label: "A person enters or leaves a segment" });
    await page.getByRole("button", { name: "Create flow", exact: true }).click();
    await page.getByLabel(/^Trigger:/).click();
    await page.getByLabel("Segment", { exact: true }).selectOption({ label: "Newsletter readers" });
    await page.getByLabel("When a person", { exact: true }).selectOption({ label: "Enters the segment" });
    await page.getByRole("checkbox", { name: "Also enroll the people already in the segment when the flow is published" }).check();
    await expect(page.getByLabel("A person who has been in the flow")).toHaveValue("once");
    await page.getByRole("complementary", { name: "Step palette" }).getByRole("button", { name: "Send an email", exact: true }).click();
    await page.getByLabel("Template", { exact: true }).selectOption({ label: NEWSLETTER });
    await page.getByLabel("Destination", { exact: true }).selectOption({ label: "Newsletter capture" });
    await expect(page.getByLabel("Topic", { exact: true })).toHaveValue("");
    await expect(page.getByRole("region", { name: "Step inspector" })).toBeVisible();
    await expect(page.getByRole("region", { name: "Flow canvas" })).toBeVisible();
    await expect(page.getByRole("dialog")).toHaveCount(0);
    await page.getByRole("complementary", { name: "Step palette" }).getByRole("button", { name: "Exit", exact: true }).click();
    await page.getByRole("region", { name: "Step inspector" }).getByRole("button", { name: "Close", exact: true }).click();
    await page.getByLabel(/^Send an email: .*step /).click();
    await page.getByLabel("Next step", { exact: true }).selectOption({ label: "Step 2: Exit, Leaves the flow" });
    await page.getByRole("region", { name: "Step inspector" }).getByRole("button", { name: "Close", exact: true }).click();
    await expect(page.getByText("Saved", { exact: true })).toBeVisible();
    await expect(page.getByText("No issues", { exact: true })).toBeVisible();
    await page.getByRole("link", { name: "Test", exact: true }).click();
    await expect(page).toHaveURL(/\?view=test$/);
    await page.getByRole("radio", { name: "A made-up person, from an editable snapshot" }).check();
    await expect(page.getByLabel("Snapshot", { exact: true })).toHaveValue("{}");
    await page.getByRole("button", { name: "Start the test", exact: true }).click();
    const path = page.getByRole("list", { name: "Steps of the test path" });
    await expect(path.getByRole("listitem")).toHaveCount(2);
    await expect(path).toContainText("Send an email");
    await expect(path).toContainText("Exit");
    await expect(path).toContainText("Completed the flow");
    await expect(page.getByRole("heading", { name: "Captured effects" })).toBeVisible();
    const effects = page.getByRole("complementary", { name: "Test timeline" }).locator(".fl-effect");
    await expect(effects).toHaveCount(1);
    await expect(effects).toContainText("Send an email");
    expect(captured()).toHaveLength(1);
    await page.getByRole("link", { name: "Build", exact: true }).click();
  });

  await test.step("8. Publish, capture the live mail and read the enrollment trace", async () => {
    await page.getByRole("button", { name: "Review and publish (0)", exact: true }).click();
    await expect(page.getByRole("dialog")).toContainText("1 person in Newsletter readers");
    await page.getByRole("dialog").getByRole("button", { name: "Publish version 1", exact: true }).click();
    await expect(page.getByRole("dialog")).toBeHidden();
    // The first newsletter would arrive within the warm-up window; let everyone in at once.
    await page.getByRole("button", { name: "Open now", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Open now", exact: true }).click();
    await expect(page.getByRole("dialog")).toBeHidden();
    const flowMessage = await receipt(page, 1, "Spring is here");
    await expect(async () => {
      await page.reload();
      await page.getByRole("link", { name: "Results", exact: true }).click();
      await expect(page).toHaveURL(/\?view=results$/);
      await expect(page.getByRole("button", { name: /^Open the enrollment of person / })).toHaveCount(1);
      await page.getByRole("button", { name: /^Open the enrollment of person .*, completed, version 1$/ }).click();
      const steps = page.getByRole("list", { name: "Steps this enrollment has run" });
      await expect(steps).toContainText("Send an email");
      await expect(steps).toContainText("Exit");
      const trace = page.getByRole("list", { name: "Decisions on the trace of this enrollment" });
      await expect(trace).toContainText("A person was considered for a flow");
      await expect(trace).toContainText("A flow step ran");
      await expect(trace).toContainText("Message sent", { timeout: 2000 });
    }).toPass({ timeout: 90_000, intervals: [1000, 2000] });
    await page.getByRole("link", { name: "Open in Traces", exact: true }).click();
    const table = page.getByRole("table", { name: /^Records for trace / });
    await expect(table).toContainText("message.send");
    const rows = table.locator("tbody tr");
    const decisions = await rows.evaluateAll((entries) => entries.map((row) => ({
      action: row.children[1]?.textContent ?? "",
      outcome: row.children[2]?.textContent ?? "",
    })));
    expect(decisions).toEqual(expect.arrayContaining([
      { action: "flow.enroll", outcome: "enrolled" },
      { action: "send.gate", outcome: "passed" },
      { action: "message.send", outcome: "sent" },
    ]));
    expect(decisions.some((record) => record.action === "flow.step")).toBe(true);
    expect(captured()).toHaveLength(2);
    const summary = process.env.GITHUB_STEP_SUMMARY;
    if (!summary) throw new Error("GITHUB_STEP_SUMMARY is required.");
    const names = [...new Set(decisions.map(({ action, outcome }) => `${action} → ${outcome}`))];
    for (const name of names) expect(name).toMatch(/^[a-z_.]+ → [a-z_]+$/);
    const evidence = [
      "\n### Getting-started walkthrough",
      "Clean docker-compose.yml stack; synthetic data; setup through publish exercised in Chromium.",
      `- Test SMTP capture: **${testMessage.subject}**; recipients: **${testMessage.recipients}**.`,
      `- Flow SMTP capture: **${flowMessage.subject}**; recipients: **${flowMessage.recipients}**.`,
      "- Both messages contain HTML, text, the postal address and an HTTPS unsubscribe link.",
      `- Enrollment trace read in the UI: ${names.map((name) => `\`${name}\``).join(", ")}.`,
      "- Newsletter editor and preview PNGs: getting-started-screenshots artifact.",
      "",
    ].join("\n");
    appendFileSync(summary, evidence);
    console.log(evidence);
  });
});
