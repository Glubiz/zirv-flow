import { describe, expect, it } from "vitest";

import { mergedSpf, recordStatus } from "./labels";

describe("sending labels", () => {
  it("says each record status in words", () => {
    expect(recordStatus("ok")).toMatchObject({ word: "OK", reason: "matches" });
    expect(recordStatus("mismatch").word).toBe("Mismatch");
    expect(recordStatus("missing").word).toBe("Missing");
    expect(recordStatus("pending").word).toBe("Pending");
    expect(recordStatus("anything else").status).toBe("pending");
  });

  it("merges the expected SPF mechanisms into the record that is already published", () => {
    expect(mergedSpf(["v=spf1 include:_spf.example.net -all"], "v=spf1 a:smtp.example.test ~all")).toBe(
      "v=spf1 include:_spf.example.net a:smtp.example.test -all",
    );
  });

  it("offers no merge when there is no SPF record or nothing to add", () => {
    expect(mergedSpf([], "v=spf1 a ~all")).toBeNull();
    expect(mergedSpf(["something else"], "v=spf1 a ~all")).toBeNull();
    expect(mergedSpf(["v=spf1 a -all"], "v=spf1 a ~all")).toBeNull();
  });
});
