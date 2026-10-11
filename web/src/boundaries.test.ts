// @vitest-environment node
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { ESLint } from "eslint";
import tseslint from "typescript-eslint";
import { describe, expect, it } from "vitest";

import { boundaryConfigs } from "../eslint.boundaries.js";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

async function lintFixture(fixture: string, pretendingToBe: string) {
  const eslint = new ESLint({
    cwd: root,
    overrideConfigFile: true,
    overrideConfig: [tseslint.configs.base, ...boundaryConfigs],
  });
  const source = readFileSync(join(root, "lint-fixtures", fixture), "utf8");
  const [result] = await eslint.lintText(source, { filePath: join(root, pretendingToBe) });
  return result?.messages ?? [];
}

describe("import boundaries", () => {
  it("fails a deep import from one feature into another feature's file", async () => {
    const messages = await lintFixture("deep-cross-feature-import.ts", "src/features/members/fixture.ts");

    expect(messages).toHaveLength(1);
    expect(messages[0]?.ruleId).toBe("no-restricted-imports");
    expect(messages[0]?.message).toContain("import the jobs feature through its index.ts");
  });

  it("fails the same deep import written as a path through src/features", async () => {
    const messages = await lintFixture("deep-cross-feature-import-by-path.ts", "src/features/members/fixture.ts");

    expect(messages.map((message) => message.ruleId)).toEqual(["no-restricted-imports"]);
  });

  it("allows importing another feature through its index", async () => {
    expect(await lintFixture("cross-feature-index-import.ts", "src/features/members/fixture.ts")).toEqual([]);
  });

  it("fails any import from src/app inside a feature", async () => {
    const messages = await lintFixture("import-from-app.ts", "src/features/members/fixture.ts");

    expect(messages[0]?.ruleId).toBe("no-restricted-imports");
    expect(messages[0]?.message).toContain("src/app is the composition root");
  });

  it("fails any import from src/app inside src/ui", async () => {
    const messages = await lintFixture("import-from-app.ts", "src/ui/fixture.ts");

    expect(messages[0]?.ruleId).toBe("no-restricted-imports");
  });

  it("fails src/ui importing a feature", async () => {
    const messages = await lintFixture("ui-imports-feature.ts", "src/ui/fixture.ts");

    expect(messages[0]?.ruleId).toBe("no-restricted-imports");
    expect(messages[0]?.message).toContain("never import a feature");
  });

  it("fails src/app reaching into a feature's file instead of its index", async () => {
    const messages = await lintFixture("app-deep-feature-import.ts", "src/app/fixture.ts");

    expect(messages[0]?.ruleId).toBe("no-restricted-imports");
    expect(messages[0]?.message).toContain("import the session feature through its index.ts");
  });
});
