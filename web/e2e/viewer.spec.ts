import { expect, test, type Browser, type Page } from "@playwright/test";

import { OWNER_EMAIL, ownerPassword, signIn } from "./support";

const VIEWER_EMAIL = "viewer@browser-test.example";
const VIEWER_PASSWORD = "viewer-password-for-the-browser-test";
const CONTACT_KEY = "viewer-check-1001";

interface Session {
  csrf_token: string;
}

/** As the owner: a viewer in the seeded workspace, and one person whose identifier the viewer must not see. */
async function seedViewer(page: Page): Promise<string> {
  await signIn(page);
  const request = page.request;
  const csrf = ((await (await request.get("/api/v1/session")).json()) as Session).csrf_token;
  const headers = { "x-csrf-token": csrf };

  const workspaces = (await (await request.get("/api/v1/workspaces")).json()) as { id: string }[];
  const workspace = workspaces[0];
  if (!workspace) throw new Error("the test server has no workspace");

  const created = await request.post("/api/v1/users", { headers, data: { email: VIEWER_EMAIL, password: VIEWER_PASSWORD } });
  expect(created.ok()).toBe(true);
  const viewer = (await created.json()) as { id: string };

  const member = await request.put(`/api/v1/workspaces/${workspace.id}/members/${viewer.id}`, { headers, data: { role: "viewer" } });
  expect(member.ok()).toBe(true);

  const contact = await request.post(`/api/v1/workspaces/${workspace.id}/contacts`, { headers, data: { key: CONTACT_KEY } });
  expect(contact.ok()).toBe(true);
  return workspace.id;
}

async function signInAsViewer(browser: Browser): Promise<Page> {
  const page = await (await browser.newContext()).newPage();
  await page.goto("/sign-in");
  await page.getByLabel("Email").fill(VIEWER_EMAIL);
  await page.getByLabel("Password").fill(VIEWER_PASSWORD);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("heading", { name: "Welcome back" })).toBeHidden();
  return page;
}

test("a viewer sees no write control on segments and flows, and no personal value in the people list", async ({ page, browser }) => {
  expect(OWNER_EMAIL).not.toBe(VIEWER_EMAIL);
  expect(ownerPassword()).not.toBe(VIEWER_PASSWORD);
  const workspace = await seedViewer(page);

  const viewer = await signInAsViewer(browser);

  await viewer.goto(`/w/${workspace}/segments`);
  await expect(viewer.getByRole("heading", { name: "Segments", level: 1 })).toBeVisible();
  await expect(viewer.getByText("You can look at segments but not change them.")).toBeVisible();
  await expect(viewer.getByRole("button", { name: /New segment/ })).toHaveCount(0);
  await expect(viewer.getByRole("link", { name: /New segment/ })).toHaveCount(0);

  await viewer.goto(`/w/${workspace}/flows`);
  await expect(viewer.getByRole("heading", { name: "Flows", level: 1 })).toBeVisible();
  await expect(viewer.getByText("You can look at flows but not change them.")).toBeVisible();
  await expect(viewer.getByRole("button", { name: /New flow/ })).toHaveCount(0);
  await expect(viewer.getByRole("link", { name: /New flow/ })).toHaveCount(0);

  await viewer.goto(`/w/${workspace}/people`);
  await viewer.getByLabel("Kind").selectOption("external");
  await viewer.getByLabel("System").fill("contact_key");
  await viewer.getByLabel("Value").fill(CONTACT_KEY);
  await viewer.getByRole("button", { name: "Search", exact: true }).click();
  const table = viewer.getByRole("table");
  await expect(table.getByText("Hidden").first()).toBeVisible();
  await expect(viewer.getByText(CONTACT_KEY, { exact: false })).toHaveCount(0);
});
