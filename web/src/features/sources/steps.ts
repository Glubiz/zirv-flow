import type { StepEntry } from "../../ui";

export type Step = "type" | "settings" | "test" | "streams";

export const STEPS: StepEntry<Step>[] = [
  { step: "type", label: "Choose a type" },
  { step: "settings", label: "Settings" },
  { step: "test", label: "Test" },
  { step: "streams", label: "Streams" },
];
