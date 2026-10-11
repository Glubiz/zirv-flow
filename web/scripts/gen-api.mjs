import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const spec = resolve(root, "../crates/zirv-flow-api/openapi.json");
const committed = resolve(root, "src/api/schema.d.ts");
const check = process.argv.includes("--check");
const bin = resolve(root, "node_modules/openapi-typescript/bin/cli.js");

const tempDir = mkdtempSync(join(tmpdir(), "zirv-flow-api-"));
const target = check ? join(tempDir, "schema.d.ts") : committed;

try {
  const run = spawnSync(process.execPath, [bin, spec, "--output", target], {
    cwd: root,
    stdio: "inherit",
  });
  if (run.status !== 0) {
    process.exit(run.status ?? 1);
  }
  const normalise = (path) => readFileSync(path, "utf8").replace(/\r\n/g, "\n");
  if (!check) {
    writeFileSync(committed, normalise(committed));
  } else if (normalise(target) !== normalise(committed)) {
    console.error(
      "src/api/schema.d.ts is out of date with crates/zirv-flow-api/openapi.json. Run: npm run gen:api",
    );
    process.exit(1);
  }
} finally {
  rmSync(tempDir, { recursive: true, force: true });
}
