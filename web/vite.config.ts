import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

const backend = "http://127.0.0.1:8080";

export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "dist",
    assetsInlineLimit: 0,
    cssCodeSplit: false,
    modulePreload: { polyfill: false },
  },
  server: {
    proxy: {
      "/api": backend,
      "/healthz": backend,
      "/readyz": backend,
    },
  },
  test: {
    environment: "jsdom",
    globals: true,
    setupFiles: ["./vitest.setup.ts"],
    include: ["src/**/*.test.{ts,tsx}", "e2e/**/*.test.ts"],
    exclude: ["node_modules/**"],
    css: false,
  },
});
