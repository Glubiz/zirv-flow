import { describe, expect, it } from "vitest";

import {
  configFrom,
  credentialFrom,
  filled,
  freshnessLabel,
  freshnessTone,
  initialValues,
  kindName,
  lastSuccessLabel,
  runErrorText,
  showValue,
  thresholdLabel,
  withoutSecrets,
} from "./labels";
import { aFreshness, FAKE_KIND } from "./testData";

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

  it("explains the codes a file run can fail with and falls back to the code", () => {
    for (const code of [
      "file_changed",
      "file_range_unsupported",
      "file_range_invalid",
      "record_too_large",
      "csv_invalid",
      "json_invalid",
      "list_missing",
      "stream_not_keyed",
      "event_key_unavailable",
    ]) {
      expect(runErrorText(code, 0.05)).not.toContain(code);
    }
    expect(runErrorText("something_new", 0.05)).toBe("The run failed (something_new).");
  });
});

describe("freshness", () => {
  it("names each state, gives it a tone and leaves an unknown state as it is", () => {
    expect([freshnessLabel("fresh"), freshnessLabel("stale"), freshnessLabel("failing")]).toEqual([
      "Fresh",
      "Stale",
      "Failing",
    ]);
    expect([freshnessTone("fresh"), freshnessTone("stale"), freshnessTone("failing")]).toEqual([
      "neutral",
      "attention",
      "error",
    ]);
    expect(freshnessLabel("paused")).toBe("paused");
  });

  it("says when a stream last succeeded, or that it never has", () => {
    const now = Date.parse("2026-10-10T12:00:00Z");

    expect(lastSuccessLabel(aFreshness({ last_success_at: "2026-10-10T09:00:00Z" }), now)).toBe("3 hours ago");
    expect(lastSuccessLabel(aFreshness({ last_success_at: null }), now)).toBe("Never");
    expect(lastSuccessLabel(null, now)).toBe("Never");
  });

  it("states the threshold in force and marks the default", () => {
    expect(thresholdLabel(aFreshness({ threshold_seconds: 7200, threshold_is_default: true }))).toBe("2.0 h (default)");
    expect(thresholdLabel(aFreshness({ threshold_seconds: 90, threshold_is_default: false }))).toBe("2 min");
    expect(thresholdLabel(aFreshness({ threshold_seconds: null }))).toBe("None");
    expect(thresholdLabel(null)).toBe("None");
  });
});
