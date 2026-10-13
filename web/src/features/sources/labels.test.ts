import { describe, expect, it } from "vitest";

import {
  configFrom,
  credentialFrom,
  filled,
  initialValues,
  kindName,
  shapesValid,
  showValue,
  withoutSecrets,
} from "./labels";
import { FAKE_KIND, POSTGRES_KIND } from "./testData";

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
});
