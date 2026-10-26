import { expect, test, type ConsoleMessage, type Page } from "@playwright/test";

import { openTemplates, signIn } from "./support";

const SCRIPT = `<script>parent.postMessage("script-ran", "*"); console.log("script-ran");</script>`;
const MARKER = "script-ran";

/** Records every `message` event the page receives, from any frame, into `window.__received`. */
async function listenForMessages(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const received: unknown[] = [];
    (window as unknown as { __received: unknown[] }).__received = received;
    window.addEventListener("message", (event) => {
      received.push(event.data);
    });
  });
}

async function received(page: Page): Promise<unknown[]> {
  return page.evaluate(() => (window as unknown as { __received: unknown[] }).__received);
}

/** Every console line of the page and of each of its frames that says the script ran. */
function watchConsole(page: Page): ConsoleMessage[] {
  const lines: ConsoleMessage[] = [];
  page.on("console", (message) => {
    if (message.text() === MARKER) lines.push(message);
  });
  return lines;
}

test("a script in a raw HTML block never runs in the preview, and the detection works", async ({ page, context }) => {
  await listenForMessages(page);
  const logged = watchConsole(page);

  await signIn(page);
  await openTemplates(page);
  await page.getByRole("button", { name: "New template" }).first().click();
  await page.getByLabel("Name").fill("Sandbox check");
  await page.getByLabel("Category").selectOption("transactional");
  await page.getByLabel(/^Blocks/).check();
  await page.getByRole("button", { name: "Create template" }).click();

  await page.getByRole("button", { name: "Add a section with 1 column" }).click();
  await page.getByRole("button", { name: "Add raw html block" }).click();
  await page.getByLabel("HTML", { exact: true }).fill(`<p>sandbox probe</p>${SCRIPT}`);

  await page.getByRole("tab", { name: "Message" }).click();
  await page.getByLabel("Sender identity").selectOption({ index: 1 });

  const frame = page.locator('iframe[title="Preview of the message"]');
  await expect(frame).toHaveAttribute("srcdoc", /sandbox probe/);
  // sandbox="" grants nothing, so no allow-scripts.
  await expect(frame).toHaveAttribute("sandbox", "");
  await expect(frame.contentFrame().getByText("sandbox probe")).toBeVisible();

  // Give a script that did run every chance to be heard.
  await page.waitForTimeout(1000);
  expect(await received(page)).not.toContain(MARKER);
  expect(logged).toEqual([]);

  // The control: the same markup in an unsandboxed frame of a blank page is heard.
  const probe = await frame.getAttribute("srcdoc");
  expect(probe).toContain("parent.postMessage");

  const control = await context.newPage();
  await listenForMessages(control);
  const controlLogged = watchConsole(control);
  await control.goto("about:blank");
  await control.evaluate((html) => {
    const unsandboxed = document.createElement("iframe");
    unsandboxed.srcdoc = html;
    document.body.append(unsandboxed);
  }, probe ?? "");
  await expect.poll(() => received(control)).toContain(MARKER);
  await expect.poll(() => controlLogged.length).toBeGreaterThan(0);
});

// The upload half of criterion 6 needs asset uploads, which arrive with #306.
test.skip("an SVG upload is refused", () => {
  // Once #306 exists: upload an SVG in the assets library and assert that it is refused.
});
