import { expect, test } from "@playwright/test";

import { openTemplates, signIn } from "./support";

test("the server boots, the owner signs in and opens the templates page", async ({ page }) => {
  await signIn(page);
  await openTemplates(page);

  await expect(page.getByRole("button", { name: "New template" }).first()).toBeVisible();
});
