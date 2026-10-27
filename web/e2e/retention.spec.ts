import { expect, test } from "@playwright/test";

import { signIn } from "./support";

test("an admin shortens the messages window and sees it after a reload", async ({ page }) => {
  await signIn(page);
  await page.getByRole("link", { name: "Privacy" }).first().click();

  const table = page.getByRole("table", { name: "Retention windows" });
  await expect(table).toBeVisible();
  await table.getByRole("button", { name: "Shorten Messages" }).click();

  const dialog = page.getByRole("dialog", { name: "Shorten a window" });
  await dialog.getByLabel("Unit").selectOption("days");
  await dialog.getByLabel("Keep for").fill("90");
  await dialog.getByRole("button", { name: "Save" }).click();
  await expect(dialog).toBeHidden();
  await expect(table.getByRole("row", { name: /Messages 90 days/ })).toBeVisible();

  await page.reload();
  await expect(page.getByRole("table", { name: "Retention windows" }).getByRole("row", { name: /Messages 90 days/ })).toBeVisible();
});
