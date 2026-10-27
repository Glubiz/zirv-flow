import { expect, type Page } from "@playwright/test";

export const OWNER_EMAIL = "owner@browser-test.example";

/** The owner's password, which Playwright generated for this run. */
export function ownerPassword(): string {
  const password = process.env.ZIRV_FLOW_BROWSER_TEST_PASSWORD;
  if (!password) throw new Error("ZIRV_FLOW_BROWSER_TEST_PASSWORD is not set");
  return password;
}

/** Signs in through the real sign-in page as the seeded owner. */
export async function signIn(page: Page): Promise<void> {
  await page.goto("/sign-in");
  await page.getByLabel("Email").fill(OWNER_EMAIL);
  await page.getByLabel("Password").fill(ownerPassword());
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("heading", { name: "Welcome back" })).toBeHidden();
}

/** Opens the templates page of the seeded workspace. */
export async function openTemplates(page: Page): Promise<void> {
  await page.getByRole("link", { name: "Templates" }).first().click();
  await expect(page.getByRole("heading", { name: "Template library", level: 1 })).toBeVisible();
}
