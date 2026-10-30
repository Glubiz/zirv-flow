import { expect, test } from "@playwright/test";

import { openTemplates, signIn } from "./support";

test("the server boots, the owner signs in and opens the templates page", async ({ page }) => {
  await signIn(page);
  await openTemplates(page);

  await expect(page.getByRole("button", { name: "New template" }).first()).toBeVisible();
});

test("sign-in lands on Home and Settings has a way back", async ({ page }) => {
  await signIn(page);
  await expect(page.getByRole("heading", { name: "Home", exact: true })).toBeVisible();
  const home = page.url();
  await page.getByRole("navigation", { name: "Main", exact: true }).getByRole("link", { name: "Settings" }).click();
  await expect(page.getByRole("heading", { name: "Settings", exact: true })).toBeVisible();
  const settings = page.getByRole("navigation", { name: "Settings", exact: true });
  await settings.getByRole("link", { name: "Members and roles" }).click();
  await expect(page.getByRole("heading", { name: "Members", exact: true })).toBeVisible();
  await settings.getByRole("link", { name: /^Back to / }).click();
  await expect(page).toHaveURL(home);
  await expect(page.getByRole("heading", { name: "Home", exact: true })).toBeVisible();
});

test("the mobile navigation drawer closes with Escape and restores focus", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await signIn(page);
  await expect(page.getByRole("heading", { name: "Home", exact: true })).toBeVisible();
  const menu = page.getByRole("button", { name: "Open menu" });
  await menu.click();
  const drawer = page.getByRole("dialog", { name: "Navigation" });
  await expect(drawer).toBeVisible();
  await expect(drawer.getByRole("navigation", { name: "Main", exact: true })).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(drawer).toBeHidden();
  await expect(menu).toBeFocused();
});
