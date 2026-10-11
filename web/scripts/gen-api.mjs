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

const METHODS = ["get", "put", "post", "delete", "patch", "head", "options", "trace"];

// openapi-typescript keys its operations table by operationId, so ids that repeat across
// modules (list, create, get) would collapse into one entry. Qualify only the repeats with
// their first tag.
function withUniqueOperationIds(document) {
  const seen = new Map();
  for (const item of Object.values(document.paths)) {
    for (const method of METHODS) {
      const id = item[method]?.operationId;
      if (id) seen.set(id, (seen.get(id) ?? 0) + 1);
    }
  }
  for (const item of Object.values(document.paths)) {
    for (const method of METHODS) {
      const operation = item[method];
      if (operation?.operationId && seen.get(operation.operationId) > 1) {
        const tag = (operation.tags?.[0] ?? method).replace(/[^A-Za-z0-9]+(.)?/g, (_, next) => (next ? next.toUpperCase() : ""));
        operation.operationId = `${tag}_${operation.operationId}`;
      }
    }
  }
  return document;
}

const tempDir = mkdtempSync(join(tmpdir(), "zirv-flow-api-"));
const target = check ? join(tempDir, "schema.d.ts") : committed;
const prepared = join(tempDir, "openapi.json");

try {
  const document = withUniqueOperationIds(JSON.parse(readFileSync(spec, "utf8")));
  writeFileSync(prepared, JSON.stringify(document));
  const run = spawnSync(process.execPath, [bin, prepared, "--output", target], {
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
