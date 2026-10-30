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
  await openPage(page, "Templates");
  await expect(page.getByRole("heading", { name: "Template library", level: 1 })).toBeVisible();
}

/** Opens a destination through the sidebar, including its folded and mobile forms. */
export async function openPage(page: Page, name: string, area: "Main" | "Settings" = "Main"): Promise<void> {
  const mobileMenu = page.getByRole("button", { name: "Open menu", exact: true });
  if (await mobileMenu.isVisible()) await mobileMenu.click();
  const unfold = page.getByRole("button", { name: "Unfold the menu", exact: true });
  if (await unfold.isVisible()) await unfold.click();
  const nav = page.getByRole("navigation", { name: area, exact: true });
  if (!(await nav.isVisible())) {
    if (area === "Settings") {
      await page.getByRole("navigation", { name: "Main", exact: true }).getByRole("link", { name: "Settings", exact: true }).click();
    } else {
      await page.getByRole("navigation", { name: "Settings", exact: true }).getByRole("link", { name: /^Back to / }).click();
    }
    if (await mobileMenu.isVisible()) await mobileMenu.click();
  }
  await nav.getByRole("link", { name, exact: true }).click();
}
