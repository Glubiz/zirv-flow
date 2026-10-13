import { describe, expect, it } from "vitest";

import {
  configFrom,
  credentialFrom,
  curlExample,
  filled,
  freshnessLabel,
  freshnessTone,
  initialValues,
  isPush,
  kindName,
  lastSuccessLabel,
  shapesValid,
  runErrorText,
  schemeSummary,
  showValue,
  thresholdLabel,
  webhookCodeText,
  webhookUrl,
  withoutSecrets,
} from "./labels";
import { aFreshness, aWebhook, FAKE_KIND, POSTGRES_KIND, WEBHOOK_KIND } from "./testData";

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

describe("webhook labels", () => {
  it("tells a push source type from a pulled one and builds the address from the origin and the path", () => {
    expect(isPush(WEBHOOK_KIND)).toBe(true);
    expect(isPush(FAKE_KIND)).toBe(false);
    expect(isPush(null)).toBe(false);
    expect(webhookUrl("https://flow.example.test", aWebhook())).toBe("https://flow.example.test/hooks/hook-1");
  });

  it("summarises both signing schemes", () => {
    expect(schemeSummary(aWebhook())).toBe(
      'HMAC-SHA256 of "timestamp.body" in X-Zirv-Signature (prefix sha256=), timestamp in X-Zirv-Timestamp, within 5 min',
    );
    expect(schemeSummary(aWebhook({ timestamp_header: null, signature_prefix: "" }))).toContain("no prefix");
    expect(schemeSummary(aWebhook({ timestamp_header: null }))).toContain("only the dedupe field stops a replay");
  });

  it("writes the example command the README documents, for either scheme", () => {
    const timestamped = curlExample("https://flow.example.test/hooks/hook-1", aWebhook(), "zfwh_secret");
    expect(timestamped).toContain(`printf '%s' "$TS.$BODY" | openssl dgst -sha256 -hmac 'zfwh_secret'`);
    expect(timestamped).toContain(`-H "X-Zirv-Timestamp: $TS"`);
    expect(timestamped).toContain(`-H "X-Zirv-Signature: sha256=$SIG"`);
    expect(timestamped).toContain(`curl -X POST 'https://flow.example.test/hooks/hook-1' \\\n  -H 'Content-Type`);
    const bodyOnly = curlExample("https://x/hooks/h", aWebhook({ timestamp_header: null }), "zfwh_secret");
    expect(bodyOnly).toContain(`printf '%s' "$BODY" | openssl dgst`);
    expect(bodyOnly).not.toContain("$TS");
  });

  it("explains the codes in plain words and shows an unknown one as it is", () => {
    expect(webhookCodeText("push_source")).toMatch(/no run/);
    expect(webhookCodeText("mapping_unavailable")).toMatch(/no usable mapping/);
    expect(webhookCodeText("something_new")).toBe("something_new");
  });
});
