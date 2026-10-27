import { expect, test, type Page } from "@playwright/test";

import { signIn } from "./support";

const ADMIN_EMAIL = "audit-admin@browser-test.example";
const ADMIN_PASSWORD = "audit-admin-password-for-the-browser-test";
const MEMBER_EMAIL = "audit-member@browser-test.example";
const MEMBER_PASSWORD = "audit-member-password-for-the-browser-test";

interface Session {
  csrf_token: string;
}

/** As the owner: an admin and an editor in the seeded workspace. */
async function seed(page: Page): Promise<string> {
  await signIn(page);
  const request = page.request;
  const csrf = ((await (await request.get("/api/v1/session")).json()) as Session).csrf_token;
  const headers = { "x-csrf-token": csrf };
  const workspaces = (await (await request.get("/api/v1/workspaces")).json()) as { id: string }[];
  const workspace = workspaces[0];
  if (!workspace) throw new Error("the test server has no workspace");

  for (const [email, password, role] of [
    [ADMIN_EMAIL, ADMIN_PASSWORD, "admin"],
    [MEMBER_EMAIL, MEMBER_PASSWORD, "editor"],
  ] as const) {
    const created = await request.post("/api/v1/users", { headers, data: { email, password } });
    expect(created.ok()).toBe(true);
    const user = (await created.json()) as { id: string };
    const member = await request.put(`/api/v1/workspaces/${workspace.id}/members/${user.id}`, { headers, data: { role } });
    expect(member.ok()).toBe(true);
  }
  return workspace.id;
}

test("an admin changes a member's role, then finds membership.set in the audit log and opens its entry", async ({ page, browser }) => {
  const workspace = await seed(page);

  const admin = await (await browser.newContext()).newPage();
  await admin.goto("/sign-in");
  await admin.getByLabel("Email").fill(ADMIN_EMAIL);
  await admin.getByLabel("Password").fill(ADMIN_PASSWORD);
  await admin.getByRole("button", { name: "Sign in" }).click();
  await expect(admin.getByRole("heading", { name: "Welcome back" })).toBeHidden();

  await admin.goto(`/w/${workspace}/members`);
  const role = admin.getByRole("combobox", { name: `Role for ${MEMBER_EMAIL}` });
  await role.selectOption("publisher");
  await expect(role).toHaveValue("publisher");

  await admin.getByRole("link", { name: "Audit log" }).click();
  await expect(admin.getByRole("heading", { name: "Audit log", level: 1 })).toBeVisible();
  await admin.getByLabel("Action", { exact: true }).selectOption("membership.set");
  await admin.getByRole("button", { name: "Apply" }).click();

  await admin.getByRole("button", { name: "Open entry membership.set" }).first().click();
  const sheet = admin.getByRole("dialog", { name: "Membership set" });
  await expect(sheet).toBeVisible();
  await expect(sheet.getByText("Changes")).toBeVisible();
  await expect(sheet.getByText("membership.set")).toBeVisible();
});
