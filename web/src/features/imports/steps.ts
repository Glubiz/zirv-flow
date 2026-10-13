import type { StepEntry } from "../../ui";

export type Step = "upload" | "map" | "dry-run" | "run";

export const STEPS: StepEntry<Step>[] = [
  { step: "upload", label: "Upload" },
  { step: "map", label: "Map" },
  { step: "dry-run", label: "Dry run" },
  { step: "run", label: "Run" },
];
