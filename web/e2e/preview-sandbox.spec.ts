import { expect, test, type ConsoleMessage, type Page } from "@playwright/test";

import { OWNER_EMAIL, openTemplates, ownerPassword, signIn } from "./support";

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

test("the preview has its own CSP and blocks scripts, top navigation, forms and base URLs", async ({ page, context }) => {
  await listenForMessages(page);
  const logged = watchConsole(page);

  await signIn(page);
  await openTemplates(page);
  await page.getByRole("button", { name: "New template" }).first().click();
  await page.getByLabel("Name").fill("Sandbox check");
  await page.getByRole("button", { name: "Transactional" }).click();
  await page.getByLabel(/^Blocks/).check();
  await page.getByRole("button", { name: "Create template" }).click();

  await page.getByRole("button", { name: "Add a section with 1 column" }).click();
  await page.getByRole("button", { name: "Add raw html block" }).click();
  await page.getByLabel("HTML", { exact: true }).fill(`<p>sandbox probe</p>${SCRIPT}
    <base href="https://preview-escape.invalid/" target="_top">
    <a href="https://preview-escape.invalid/link" target="_top">Escape the preview</a>
    <form action="https://preview-escape.invalid/submitted" method="post"><button type="submit">Submit probe</button></form>`);

  await page.getByRole("tab", { name: "Message" }).click();
  await page.getByLabel("Sender identity").selectOption({ index: 1 });

  const frame = page.locator('iframe[title="Preview of the message"]');
  await expect(frame).toHaveAttribute("srcdoc", /sandbox probe/);
  // sandbox="" grants nothing, so no allow-scripts.
  await expect(frame).toHaveAttribute("sandbox", "");
  await expect(frame.contentFrame().getByText("sandbox probe")).toBeVisible();
  const preview = frame.contentFrame();
  const policy = preview.locator("head > :first-child");
  await expect(policy).toHaveAttribute("http-equiv", "Content-Security-Policy");
  await expect(policy).toHaveAttribute("content", "default-src 'none'; img-src https: data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'");

  const topUrl = page.url();
  await expect(preview.locator("base")).toHaveAttribute("href", "https://preview-escape.invalid/");
  expect(await preview.locator("html").evaluate((element) => element.ownerDocument.baseURI)).toBe(topUrl);
  const navigations: string[] = [];
  page.on("request", (request) => {
    if (request.isNavigationRequest()) navigations.push(request.url());
  });
  await preview.getByRole("link", { name: "Escape the preview" }).click();
  await preview.getByRole("button", { name: "Submit probe" }).click();

  // Give a script that did run every chance to be heard.
  await page.waitForTimeout(1000);
  expect(await received(page)).not.toContain(MARKER);
  expect(logged).toEqual([]);
  await expect(page).toHaveURL(topUrl);
  await expect(preview.getByText("sandbox probe")).toBeVisible();
  expect(navigations).toEqual([]);

  // The control removes both restrictions, so the same script must be heard.
  const probe = await frame.getAttribute("srcdoc");
  expect(probe).toContain("parent.postMessage");

  const control = await context.newPage();
  await listenForMessages(control);
  const controlLogged = watchConsole(control);
  await control.goto("about:blank");
  await control.evaluate((html) => {
    const parsed = new DOMParser().parseFromString(html, "text/html");
    parsed.querySelectorAll('meta[http-equiv="Content-Security-Policy"]').forEach((meta) => meta.remove());
    const unsandboxed = document.createElement("iframe");
    unsandboxed.srcdoc = parsed.documentElement.outerHTML;
    document.body.append(unsandboxed);
  }, probe ?? "");
  await expect.poll(() => received(control)).toContain(MARKER);
  await expect.poll(() => controlLogged.length).toBeGreaterThan(0);
});

// The upload half of criterion 6: an SVG is refused by its content type, before its body is read.
test("an SVG upload is refused", async ({ request }) => {
  const session = await request.post("/api/v1/session", { data: { email: OWNER_EMAIL, password: ownerPassword() } });
  expect(session.ok()).toBe(true);
  const { csrf_token: csrf } = (await session.json()) as { csrf_token: string };
  const workspaces = (await (await request.get("/api/v1/workspaces")).json()) as { id: string }[];
  const workspace = workspaces[0];
  if (!workspace) throw new Error("the seeded workspace is missing");

  const reply = await request.post(`/api/v1/workspaces/${workspace.id}/assets?name=logo.svg`, {
    headers: { "content-type": "image/svg+xml", "x-csrf-token": csrf },
    data: Buffer.from('<svg xmlns="http://www.w3.org/2000/svg"><script>parent.postMessage("script-ran", "*")</script></svg>'),
  });

  expect(reply.status()).toBe(415);
  expect(((await reply.json()) as { code: string }).code).toBe("asset_type_unsupported");
});
