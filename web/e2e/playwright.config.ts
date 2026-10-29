import { randomBytes } from "node:crypto";
import { resolve } from "node:path";

import { defineConfig, devices } from "@playwright/test";

// A password for this run alone. It is set before the test server starts and reaches the server
// and the workers through the environment; it is never written down, printed or committed. A
// worker evaluates this file again and finds the value already set.
process.env.ZIRV_FLOW_BROWSER_TEST_PASSWORD ??= randomBytes(24).toString("hex");

const port = process.env.ZIRV_FLOW_BROWSER_TEST_PORT ?? "47700";
const binary =
  process.env.ZIRV_FLOW_BROWSER_TEST_BIN ??
  resolve(import.meta.dirname, "../../target/debug", process.platform === "win32" ? "zirv-flow-browser-test.exe" : "zirv-flow-browser-test");

export default defineConfig({
  testDir: ".",
  testMatch: "**/*.spec.ts",
  testIgnore: "**/guide/**",
  globalTeardown: "./global-teardown.ts",
  fullyParallel: false,
  workers: 1,
  forbidOnly: Boolean(process.env.CI),
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [["list"], ["html", { open: "never", outputFolder: "playwright-report" }]] : "list",
  outputDir: "test-results",
  use: {
    baseURL: `http://127.0.0.1:${port}`,
    // A failure trace or report can contain the per-run password, which is valid only for that
    // run's scratch database.
    trace: "on-first-retry",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  // The test server of the `browser-test` cargo feature: it prints `READY <url>` when it serves.
  // DATABASE_URL names the PostgreSQL it makes its scratch database on.
  webServer: {
    command: `"${binary}"`,
    env: {
      DATABASE_URL: process.env.DATABASE_URL ?? "postgres://postgres:postgres@127.0.0.1:5432/postgres",
      ZIRV_FLOW_BROWSER_TEST_PORT: port,
      ZIRV_FLOW_BROWSER_TEST_PASSWORD: process.env.ZIRV_FLOW_BROWSER_TEST_PASSWORD,
    },
    wait: { stdout: /^READY http:\/\/127\.0\.0\.1:\d+/m },
    gracefulShutdown: { signal: "SIGTERM", timeout: 10_000 },
    timeout: 120_000,
    reuseExistingServer: false,
    stderr: "pipe",
  },
});
