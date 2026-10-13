import { describe, expect, it } from "vitest";

import {
  configFrom,
  describeJson,
  issueText,
  MANIFEST_ISSUE_CODES,
  oauthReasonText,
  parseJsonObject,
  credentialFrom,
  filled,
  freshnessLabel,
  freshnessTone,
  initialValues,
  kindName,
  lastSuccessLabel,
  shapesValid,
  runErrorText,
  showValue,
  thresholdLabel,
  withoutSecrets,
} from "./labels";
import { aFreshness, FAKE_KIND, POSTGRES_KIND } from "./testData";

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

  it("sends the queries of a database source as a list and leaves an empty list out", () => {
    const fields = POSTGRES_KIND.config_fields;
    const queries = JSON.stringify([{ name: "vip", sql: "SELECT id FROM customers" }]);

    expect(configFrom(fields, { host: "db.example.com", port: "5433", queries })).toEqual({
      host: "db.example.com",
      port: 5433,
      queries: [{ name: "vip", sql: "SELECT id FROM customers" }],
    });
    expect(configFrom(fields, { host: "db", queries: "[]" })).toEqual({ host: "db" });
    expect(configFrom(fields, { host: "db", queries: "" })).toEqual({ host: "db" });
    expect(configFrom(fields, { ca_certificate: "  -----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----\n" })).toEqual({
      ca_certificate: "-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----",
    });
  });

  it("shows the stored queries as the JSON text the editor reads", () => {
    const fields = POSTGRES_KIND.config_fields;
    const stored = [{ name: "vip", sql: "SELECT 1" }];

    const values = initialValues(fields, { host: "db", queries: stored });

    expect(values.queries).toBe(JSON.stringify(stored));
    expect(configFrom(fields, values)).toEqual({ host: "db", queries: stored });
  });

  it("blocks a form whose queries are not valid and passes one without queries", () => {
    const fields = POSTGRES_KIND.config_fields;

    expect(shapesValid(fields, {})).toBe(true);
    expect(shapesValid(fields, { queries: JSON.stringify([{ name: "vip", sql: "SELECT 1" }]) })).toBe(true);
    expect(shapesValid(fields, { queries: JSON.stringify([{ name: "Vip!", sql: "SELECT 1" }]) })).toBe(false);
    expect(shapesValid(fields, { queries: JSON.stringify([{ name: "vip", sql: "SELECT 1;" }]) })).toBe(false);
    expect(shapesValid(fields, { queries: "not json" })).toBe(false);
    expect(shapesValid(CONFIG, { streams: "2" })).toBe(true);
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

describe("the manifest settings", () => {
  const FIELDS = [{ ...CONFIG[0]!, key: "manifest", kind: "json", required: true }];

  it("keeps a JSON object as indented text in the form and sends it back as an object", () => {
    const values = initialValues(FIELDS, { manifest: { version: "v1", streams: [] } });

    expect(values.manifest).toBe('{\n  "version": "v1",\n  "streams": []\n}');
    expect(configFrom(FIELDS, values)).toEqual({ manifest: { version: "v1", streams: [] } });
  });

  it("leaves out text that is not an object and refuses to save it", () => {
    for (const text of ["[1]", "5", '"x"', "{", ""]) {
      expect(parseJsonObject(text), text).toBeNull();
      expect(configFrom(FIELDS, { manifest: text }), text).toEqual({});
    }
    expect(shapesValid(FIELDS, { manifest: "[1]" })).toBe(false);
    expect(shapesValid(FIELDS, { manifest: "{}" })).toBe(true);
  });

  it("describes a manifest by its streams", () => {
    expect(describeJson({ streams: [{}, {}] })).toBe("2 streams described");
    expect(describeJson({ streams: [{}] })).toBe("1 stream described");
    expect(describeJson({})).toBe("Set");
    expect(describeJson(null)).toBe("Set");
  });

  it("has a sentence for every code the validator can return, and says so plainly for an unknown one", () => {
    const unknown = issueText("a_code_from_the_future");
    for (const code of MANIFEST_ISSUE_CODES) {
      const text = issueText(code);
      expect(text, code).not.toBe("");
      expect(text, code).not.toBe(unknown);
    }
    expect(unknown).toBe("This part of the manifest is not valid.");
  });

  it("explains every reason a connection can fail, and a reason it does not know", () => {
    for (const reason of [
      "unknown_state",
      "state_mismatch",
      "provider_error",
      "exchange_failed",
      "no_refresh_token",
      "source_gone",
    ]) {
      expect(oauthReasonText(reason), reason).not.toBe(oauthReasonText("nope"));
    }
    expect(oauthReasonText(null)).toBe("Connecting the account did not work. Try again.");
  });

  it("explains a run that stopped on a manifest that no longer passes", () => {
    expect(runErrorText("source_config_invalid", 0.05)).toContain("manifest");
  });
});
