import { describe, expect, it } from "vitest";

import { issueSentence } from "./labels";

describe("issueSentence", () => {
  it("says a segment's unknown field may be archived, since the API sends the same code for both", () => {
    expect(issueSentence({ code: "unknown_trait", at: "/all/0/trait" }, "segment")).toContain("or has been archived");
  });
});
