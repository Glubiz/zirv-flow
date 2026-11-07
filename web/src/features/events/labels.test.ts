import { describe, expect, it } from "vitest";

import { reasonLabel } from "./labels";

describe("reasonLabel", () => {
  it.each([
    "missing_required",
    "wrong_type",
    "not_in_enum",
    "unplanned_property",
    "invalid_key",
    "archived_key",
    "too_long",
    "not_inferable",
  ])("has a sentence for the stored code %s", (code) => {
    expect(reasonLabel(code)).not.toBe(code);
  });

  it("falls back to the code it does not know", () => {
    expect(reasonLabel("new_code")).toBe("new_code");
  });
});
