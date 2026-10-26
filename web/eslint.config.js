import { dirname } from "node:path";
import { fileURLToPath } from "node:url";

import js from "@eslint/js";
import reactHooks from "eslint-plugin-react-hooks";
import globals from "globals";
import tseslint from "typescript-eslint";

import { boundaryConfigs } from "./eslint.boundaries.js";

const here = dirname(fileURLToPath(import.meta.url));
const typed = ["src/**/*.{ts,tsx}", "e2e/**/*.ts", "vite.config.ts", "vitest.setup.ts"];

export default tseslint.config(
  { ignores: ["dist", "node_modules", "lint-fixtures", "src/api/schema.d.ts"] },
  js.configs.recommended,
  ...tseslint.configs.strictTypeChecked.map((config) => ({ ...config, files: typed })),
  {
    files: typed,
    languageOptions: {
      globals: { ...globals.browser },
      parserOptions: { projectService: true, tsconfigRootDir: here },
    },
    plugins: { "react-hooks": reactHooks },
    rules: {
      ...reactHooks.configs.flat.recommended.rules,
      "@typescript-eslint/consistent-type-imports": ["error", { fixStyle: "inline-type-imports" }],
      "@typescript-eslint/restrict-template-expressions": ["error", { allowNumber: true }],
      "@typescript-eslint/no-confusing-void-expression": ["error", { ignoreArrowShorthand: true }],
    },
  },
  {
    files: ["scripts/**/*.mjs", "*.js", "e2e/**/*.ts"],
    languageOptions: { globals: { ...globals.node } },
  },
  ...boundaryConfigs,
  {
    files: ["src/**/*.test.{ts,tsx}", "src/test/**/*.{ts,tsx}"],
    rules: {
      "@typescript-eslint/no-non-null-assertion": "off",
      "@typescript-eslint/unbound-method": "off",
    },
  },
);
