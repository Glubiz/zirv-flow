import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
  testDir: ".",
  testMatch: "getting-started.spec.ts",
  workers: 1,
  retries: 0,
  forbidOnly: true,
  timeout: 300_000,
  expect: { timeout: 30_000 },
  reporter: "list",
  outputDir: "../test-results/guide-run",
  use: {
    ...devices["Desktop Chrome"],
    baseURL: "https://localhost:8443",
    viewport: { width: 1440, height: 1000 },
    // The ephemeral CA is trusted by SMTP and curl; Chromium accepts this local fixture.
    ignoreHTTPSErrors: true,
    trace: "off",
    screenshot: "off",
    video: "off",
  },
});
