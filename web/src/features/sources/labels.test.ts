import { describe, expect, it } from "vitest";

import { configFrom, credentialFrom, filled, initialValues, kindName, showValue, withoutSecrets } from "./labels";
import { FAKE_KIND } from "./testData";

const CONFIG = FAKE_KIND.config_fields;
const CREDENTIAL = FAKE_KIND.credential_fields;

describe("source forms", () => {
  it("starts empty, fills settings from the stored configuration and never fills a secret", () => {
    const fields = [...CONFIG, ...CREDENTIAL];

    expect(initialValues(fields)).toEqual({ streams: "", fail_check: "", token: "" });
    expect(initialValues(fields, { streams: 3, fail_check: true, token: "leaked" })).toEqual({
      streams: "3",
      fail_check: "true",
      token: "",
    });
  });

  it("types numbers and booleans and leaves out empty optional settings", () => {
    expect(configFrom(CONFIG, { streams: " 4 ", fail_check: "true" })).toEqual({ streams: 4, fail_check: true });
    expect(configFrom(CONFIG, { streams: "", fail_check: "false" })).toEqual({ fail_check: false });
    expect(configFrom(CONFIG, {})).toEqual({});
  });

  it("sends only the credential fields that were typed", () => {
    expect(credentialFrom(CREDENTIAL, { token: "ok" })).toEqual({ token: "ok" });
    expect(credentialFrom(CREDENTIAL, { token: "" })).toEqual({});
  });

  it("is filled when every required field has a value", () => {
    expect(filled(CREDENTIAL, { token: "  " })).toBe(false);
    expect(filled(CREDENTIAL, { token: "ok" })).toBe(true);
    expect(filled(CONFIG, {})).toBe(true);
  });

  it("empties the secret fields and keeps the rest", () => {
    const fields = [...CONFIG, ...CREDENTIAL];

    expect(withoutSecrets(fields, { streams: "2", token: "ok" })).toEqual({ streams: "2", token: "" });
  });

  it("shows text, numbers and booleans as they are and anything else as JSON", () => {
    expect(showValue("a")).toBe("a");
    expect(showValue(2)).toBe("2");
    expect(showValue(false)).toBe("false");
    expect(showValue({ a: 1 })).toBe('{"a":1}');
    expect(showValue(null)).toBe("");
  });

  it("names a type by its label and falls back to its id", () => {
    expect(kindName([FAKE_KIND], "fake")).toBe("Fake source");
    expect(kindName([FAKE_KIND], "other")).toBe("other");
    expect(kindName(null, "fake")).toBe("fake");
  });
});
