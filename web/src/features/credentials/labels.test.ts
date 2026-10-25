import { describe, expect, it } from "vitest";

import {
  complete,
  covers,
  emptyValues,
  fieldNames,
  kindName,
  maskedHint,
  publicFrom,
  schemaFromHints,
  secretFrom,
  testOutcomeLabel,
  type Credential,
  type Schema,
} from "./labels";

const SMTP: Schema = {
  fields: [
    { key: "host", label: "Host", type: "string", required: true, secret: false, options: [] },
    { key: "port", label: "Port", type: "integer", required: true, secret: false, options: [] },
    { key: "tls", label: "TLS mode", type: "enum", required: true, secret: false, options: ["starttls", "tls", "none"] },
    { key: "note", label: "Note", type: "string", required: false, secret: false, options: [] },
    { key: "password", label: "Password", type: "string", required: true, secret: true, options: [] },
  ],
};

function stored(overrides: Partial<Credential> = {}): Credential {
  return {
    id: "c1",
    key: "mail",
    name: "Mail",
    kind: "smtp",
    schema_version: 1,
    schema: SMTP,
    fields: { host: "h", port: 587 },
    hints: { password: "wxyz" },
    same_secret_as: [],
    used_by: [],
    rotated_at: "2026-10-22T08:00:00Z",
    archived: false,
    created_at: "2026-10-22T08:00:00Z",
    updated_at: "2026-10-22T08:00:00Z",
    ...overrides,
  };
}

describe("credential labels", () => {
  it("starts the non-secret fields from the stored values and every secret field empty", () => {
    expect(emptyValues(SMTP, "public", { host: "h", port: 587 })).toEqual({ host: "h", port: "587", tls: "", note: "" });
    expect(emptyValues(SMTP, "secret", { password: "never shown" })).toEqual({ password: "" });
  });

  it("is complete when every required field of the group has text", () => {
    expect(complete(SMTP, "public", { host: "h", port: "587", tls: "tls", note: "" })).toBe(true);
    expect(complete(SMTP, "public", { host: "h", port: " ", tls: "tls", note: "" })).toBe(false);
    expect(complete(SMTP, "secret", { password: "" })).toBe(false);
  });

  it("sends integers typed, leaves empty optional fields out and keeps secrets exactly as typed", () => {
    const values = { host: " h ", port: "587", tls: "tls", note: "", password: " p w " };

    expect(publicFrom(SMTP, values)).toEqual({ host: "h", port: 587, tls: "tls" });
    expect(secretFrom(SMTP, values)).toEqual({ password: " p w " });
    expect(secretFrom(SMTP, { password: "" })).toEqual({});
  });

  it("builds a schema from an implementation's own credential fields", () => {
    const schema = schemaFromHints([
      { key: "user", label: "User", required: true, secret: true },
      { key: "mode", label: "Mode", required: false, secret: false, kind: "select", options: ["a", "b"] },
      { key: "port", label: "Port", required: false, secret: false, kind: "number" },
    ]);

    expect(schema.fields.map((field) => [field.key, field.type, field.secret])).toEqual([
      ["user", "string", true],
      ["mode", "enum", false],
      ["port", "integer", false],
    ]);
    expect(schema.fields[1]?.options).toEqual(["a", "b"]);
  });

  it("lists the field names a credential holds and what it covers", () => {
    const credential = stored();

    expect(fieldNames(credential).sort()).toEqual(["host", "password", "port"]);
    const hint = (key: string, required: boolean) => ({ key, required });
    expect(covers(credential, [hint("host", true), hint("password", true)])).toBe(true);
    expect(covers(credential, [hint("token", true)])).toBe(false);
    expect(covers(credential, [])).toBe(false);
  });

  it("offers a credential only when the owner reads at least one of its fields", () => {
    const tls = stored({ kind: "tls_client", fields: { tls_client_cert: "pem" }, hints: { tls_client_key: "wxyz" } });
    const bearer = stored({ kind: "bearer_token", fields: {}, hints: { token: "wxyz" } });
    const fileSource = [{ key: "token", required: false }];

    expect(covers(tls, fileSource)).toBe(false);
    expect(covers(bearer, fileSource)).toBe(true);
  });

  it("asks only for the required fields of an owner whose manifest may declare more", () => {
    const custom = stored({ kind: "custom", fields: {}, hints: { account_secret: "wxyz" } });

    expect(covers(custom, [{ key: "token", required: false }])).toBe(false);
    expect(covers(custom, [{ key: "token", required: false }], true)).toBe(true);
    expect(covers(custom, [{ key: "token", required: true }], true)).toBe(false);
  });

  it("masks a hint and names kinds and outcomes", () => {
    expect(maskedHint("wxyz")).toBe("••••wxyz");
    expect(maskedHint("")).toBe("••••••••");
    expect(kindName([{ id: "smtp", label: "SMTP server", schema: SMTP }], "smtp")).toBe("SMTP server");
    expect(kindName(null, "custom")).toBe("Custom");
    expect(kindName(null, "imported")).toBe("Imported");
    expect(kindName(null, "other")).toBe("other");
    expect(testOutcomeLabel("ok")).toBe("Passed");
    expect(testOutcomeLabel("failed")).toBe("Failed");
    expect(testOutcomeLabel(null)).toBe("Not tested");
  });
});
