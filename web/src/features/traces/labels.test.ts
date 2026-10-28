import { describe, expect, it } from "vitest";

import registry from "./fixtures/decision-actions.json";
import { ACTION_LABELS, actionLabel } from "./labels";

describe("the decision action labels", () => {
  it("name every action of the registry", () => {
    const missing = registry.actions.filter((action) => !ACTION_LABELS[action]);
    expect(missing).toEqual([]);
  });

  it("name no action the registry does not have", () => {
    const extra = Object.keys(ACTION_LABELS).filter((action) => !registry.actions.includes(action));
    expect(extra).toEqual([]);
  });

  it("fails on an action with no label", () => {
    const withoutLabel = [...registry.actions, "made.up"].filter((action) => !ACTION_LABELS[action]);
    expect(withoutLabel).toEqual(["made.up"]);
    expect(actionLabel("made.up")).toBeNull();
    expect(actionLabel("flow.step")).toBe("A flow step ran");
  });
});
