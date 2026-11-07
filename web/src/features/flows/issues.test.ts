import { describe, expect, it } from "vitest";

import { issueText } from "./issues";

describe("issueText", () => {
  it("points a missing webhook signing secret at the Destinations page, where it is created", () => {
    const text = issueText({ code: "webhook_secret_missing", node: "hook" });

    expect(text).toContain("Destinations page");
    expect(text).not.toContain("workspace settings");
  });
});
