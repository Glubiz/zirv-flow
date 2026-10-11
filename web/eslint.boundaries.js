import { readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

export const features = readdirSync(join(here, "src/features"), { withFileTypes: true })
  .filter((entry) => entry.isDirectory())
  .map((entry) => entry.name);

const NO_APP = {
  group: ["**/app", "**/app/**"],
  message: "Boundary: src/app is the composition root. Features, ui and api never import from it.",
};

const NO_FEATURES = {
  group: ["**/features", "**/features/**"],
  message: "Boundary: src/ui and src/api are shared and never import a feature.",
};

function throughIndex(name, relative) {
  return {
    group: [`${relative}/${name}/*`, `!${relative}/${name}/index`],
    message: `Boundary: import the ${name} feature through its index.ts, never one of its files.`,
  };
}

function otherFeatures(own) {
  return features
    .filter((name) => name !== own)
    .flatMap((name) => [throughIndex(name, ".."), throughIndex(name, "**/features")]);
}

const restrict = (patterns) => ({ "no-restricted-imports": ["error", { patterns }] });

export const boundaryConfigs = [
  {
    files: ["src/app/**/*.{ts,tsx}"],
    rules: restrict(features.map((name) => throughIndex(name, "**/features"))),
  },
  {
    files: ["src/ui/**/*.{ts,tsx}", "src/api/**/*.{ts,tsx}"],
    rules: restrict([NO_APP, NO_FEATURES]),
  },
  ...features.map((name) => ({
    files: [`src/features/${name}/**/*.{ts,tsx}`],
    rules: restrict([NO_APP, ...otherFeatures(name)]),
  })),
];
