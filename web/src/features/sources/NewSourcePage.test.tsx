import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { NewSourcePage } from "./NewSourcePage";
import { aCredentialHolding, aSource, aStream, FAKE_KIND, HTTP_KIND, POSTGRES_KIND } from "./testData";

function members(role: "admin" | "member", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function mount() {
  return renderRoute("/w/ws1/sources/new", "/w/:ws/sources/new", <NewSourcePage />);
}

const CREDENTIALS = [
  aCredentialHolding("cred-1", "CRM token", ["token"]),
  aCredentialHolding("cred-2", "Other CRM token", ["token"]),
  aCredentialHolding("cred-pg", "Warehouse login", ["username", "password"]),
];

const BASE = {
  "GET /api/v1/session": () => json(session(OWNER, "csrf-sources")),
  "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
  "GET /api/v1/source-kinds": () => json([FAKE_KIND]),
  "GET /api/v1/workspaces/ws1/credentials": () => json(CREDENTIALS),
};

async function fillSettings(user: ReturnType<typeof userEvent.setup>, credential = "cred-1") {
  await user.click(await screen.findByRole("button", { name: "Choose Fake source" }));
  await user.type(screen.getByLabelText("Name"), "CRM");
  await user.type(screen.getByLabelText("Streams (optional)"), "2");
  await user.selectOptions(await screen.findByLabelText("Credential"), credential);
}

describe("the source wizard", () => {
  it("creates a source, tests it, discovers its streams and sets one up", async () => {
    let configured = aStream();
    const calls = mockApi({
      ...BASE,
      "POST /api/v1/workspaces/ws1/sources": () => json(aSource(), 201),
      "POST /api/v1/workspaces/ws1/sources/src-1/test": () => json({ ok: true, error: null }),
      "POST /api/v1/workspaces/ws1/sources/src-1/discover": () => json([aStream(), aStream({ id: "str-2", name: "stream_2" })]),
      "PUT /api/v1/workspaces/ws1/sources/src-1/streams/stream_1": async (request) => {
        const body = (await request.json()) as { enabled: boolean; sync_mode: string };
        configured = aStream({ enabled: body.enabled, sync_mode: body.sync_mode });
        return json(configured);
      },
    });
    const user = userEvent.setup();
    mount();

    await fillSettings(user);
    await user.click(screen.getByRole("button", { name: "Save and test" }));

    expect(await screen.findByText("The connection to CRM works.")).toBeInTheDocument();
    const created = calls.find((call) => call.method === "POST" && call.path.endsWith("/sources"));
    expect(created?.body).toEqual({ kind: "fake", name: "CRM", config: { streams: 2 }, credential_id: "cred-1" });
    expect(created?.headers.get("x-csrf-token")).toBe("csrf-sources");

    await user.click(screen.getByRole("button", { name: "Continue to streams" }));
    await user.click(await screen.findByRole("button", { name: "Discover streams" }));
    const table = await screen.findByRole("table", { name: "Streams of this source" });
    expect(within(table).getAllByText("Not read")).toHaveLength(2);

    await user.click(within(table).getByRole("button", { name: "Set up stream_1" }));
    const dialog = within(screen.getByRole("dialog", { name: "Set up stream_1" }));
    await user.click(dialog.getByLabelText("Read this stream"));
    await user.selectOptions(dialog.getByLabelText("Sync mode"), "incremental");
    await user.click(dialog.getByRole("button", { name: "Save stream" }));

    expect(await within(table).findByText("Read")).toBeInTheDocument();
    expect(within(table).getByText("Incremental")).toBeInTheDocument();
    const put = calls.find((call) => call.method === "PUT");
    expect(put?.body).toEqual({
      enabled: true,
      sync_mode: "incremental",
      cursor_field: "updated_at",
      primary_key: ["id"],
      staleness_threshold_seconds: null,
      reconcile_schedule: null,
      sweep_limit: 0.5,
    });
    expect(screen.getByRole("link", { name: "Finish" })).toHaveAttribute("href", "/w/ws1/sources/src-1");
  });

  it("offers only the credentials that hold the fields the type needs, and a new one opens a password input that is never filled in", async () => {
    mockApi(BASE);
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Choose Fake source" }));

    const picker = await screen.findByLabelText("Credential");
    expect(within(picker).getAllByRole("option").map((option) => option.textContent)).toEqual([
      "Choose a credential",
      "CRM token (Custom)",
      "Other CRM token (Custom)",
    ]);
    await user.click(screen.getByRole("button", { name: "New credential" }));
    const dialog = within(screen.getByRole("dialog", { name: "New credential for Fake source" }));
    const token = dialog.getByLabelText("Token");
    expect(token).toHaveAttribute("type", "password");
    expect(token).toHaveValue("");
  });

  it("will not save before the name and the required credential are filled in", async () => {
    mockApi(BASE);
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Choose Fake source" }));
    const save = screen.getByRole("button", { name: "Save and test" });
    expect(save).toBeDisabled();
    await user.type(screen.getByLabelText("Name"), "CRM");
    expect(save).toBeDisabled();
    await user.selectOptions(await screen.findByLabelText("Credential"), "cred-1");
    expect(save).toBeEnabled();
  });

  it("shows a failed test and lets the person go back, choose another credential and retry", async () => {
    let tests = 0;
    const calls = mockApi({
      ...BASE,
      "POST /api/v1/workspaces/ws1/sources": () => json(aSource(), 201),
      "PATCH /api/v1/workspaces/ws1/sources/src-1": () => json(aSource()),
      "POST /api/v1/workspaces/ws1/sources/src-1/test": () => {
        tests += 1;
        return tests === 1
          ? json({ ok: false, error: "the source refused the credential" })
          : json({ ok: true, error: null });
      },
    });
    const user = userEvent.setup();
    mount();

    await fillSettings(user, "cred-1");
    await user.click(screen.getByRole("button", { name: "Save and test" }));

    expect(await screen.findByText("The connection test failed")).toBeInTheDocument();
    expect(screen.getByText("the source refused the credential")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Continue to streams" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Back to settings" }));

    expect(screen.getByLabelText("Name")).toHaveValue("CRM");
    await user.selectOptions(await screen.findByLabelText("Credential"), "cred-2");
    await user.click(screen.getByRole("button", { name: "Save and test" }));

    expect(await screen.findByText("The connection to CRM works.")).toBeInTheDocument();
    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.body).toEqual({ name: "CRM", config: { streams: 2 }, credential_id: "cred-2" });
    expect(calls.filter((call) => call.method === "POST" && call.path.endsWith("/sources"))).toHaveLength(1);
  });

  it("keeps the chosen credential on a retry", async () => {
    let tests = 0;
    const calls = mockApi({
      ...BASE,
      "POST /api/v1/workspaces/ws1/sources": () => json(aSource(), 201),
      "PATCH /api/v1/workspaces/ws1/sources/src-1": () => json(aSource()),
      "POST /api/v1/workspaces/ws1/sources/src-1/test": () => {
        tests += 1;
        return json({ ok: tests > 1, error: tests > 1 ? null : "the source could not be reached" });
      },
    });
    const user = userEvent.setup();
    mount();

    await fillSettings(user);
    await user.click(screen.getByRole("button", { name: "Save and test" }));
    await user.click(await screen.findByRole("button", { name: "Back to settings" }));
    await user.click(screen.getByRole("button", { name: "Save and test" }));

    expect(await screen.findByText("The connection to CRM works.")).toBeInTheDocument();
    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.body).toEqual({ name: "CRM", config: { streams: 2 }, credential_id: "cred-1" });
  });

  it("offers the database types and creates a PostgreSQL source with a CA and a query", async () => {
    const calls = mockApi({
      ...BASE,
      "GET /api/v1/source-kinds": () => json([FAKE_KIND, POSTGRES_KIND]),
      "POST /api/v1/workspaces/ws1/sources": () => json(aSource({ kind: "postgres", name: "Warehouse" }), 201),
      "POST /api/v1/workspaces/ws1/sources/src-1/test": () => json({ ok: true, error: null }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Choose PostgreSQL" }));
    expect(screen.getByRole("link", { name: "How to create one" })).toHaveAttribute(
      "href",
      "https://github.com/Glubiz/zirv-flow#database-sources",
    );
    expect(screen.getByText("No queries")).toBeInTheDocument();
    await user.type(screen.getByLabelText("Name"), "Warehouse");
    await user.type(screen.getByLabelText("Host"), "db.example.com");
    await user.type(screen.getByLabelText("Database"), "shop");
    await user.selectOptions(screen.getByLabelText("TLS (optional)"), "verify");
    await user.click(screen.getByLabelText("CA certificate (optional)"));
    await user.paste("-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----");
    await user.click(screen.getByRole("button", { name: "Add query" }));
    await user.type(screen.getByLabelText("Query 1 name"), "vip");
    const save = screen.getByRole("button", { name: "Save and test" });
    await user.selectOptions(await screen.findByLabelText("Credential"), "cred-pg");
    // An unfinished query keeps the form from being saved.
    expect(save).toBeDisabled();
    await user.type(screen.getByLabelText("Query 1 SQL"), "SELECT id, email FROM customers");
    expect(save).toBeEnabled();
    await user.click(save);

    expect(await screen.findByText("The connection to Warehouse works.")).toBeInTheDocument();
    const created = calls.find((call) => call.method === "POST" && call.path.endsWith("/sources"));
    expect(created?.body).toEqual({
      kind: "postgres",
      name: "Warehouse",
      config: {
        host: "db.example.com",
        database: "shop",
        tls: "verify",
        ca_certificate: "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----",
        queries: [{ name: "vip", sql: "SELECT id, email FROM customers" }],
      },
      credential_id: "cred-pg",
    });
  });

  it("round-trips the queries and the CA when the settings are opened again", async () => {
    const calls = mockApi({
      ...BASE,
      "GET /api/v1/source-kinds": () => json([POSTGRES_KIND]),
      "POST /api/v1/workspaces/ws1/sources": () => json(aSource({ kind: "postgres" }), 201),
      "PATCH /api/v1/workspaces/ws1/sources/src-1": () => json(aSource({ kind: "postgres" })),
      "POST /api/v1/workspaces/ws1/sources/src-1/test": () => json({ ok: false, error: "the source could not be reached" }),
    });
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("button", { name: "Choose PostgreSQL" }));
    await user.type(screen.getByLabelText("Name"), "Warehouse");
    await user.type(screen.getByLabelText("Host"), "db.example.com");
    await user.type(screen.getByLabelText("Database"), "shop");
    await user.click(screen.getByRole("button", { name: "Add query" }));
    await user.type(screen.getByLabelText("Query 1 name"), "vip");
    await user.type(screen.getByLabelText("Query 1 SQL"), "SELECT 1");
    await user.selectOptions(await screen.findByLabelText("Credential"), "cred-pg");
    await user.click(screen.getByRole("button", { name: "Save and test" }));

    await user.click(await screen.findByRole("button", { name: "Back to settings" }));

    expect(screen.getByLabelText("Query 1 name")).toHaveValue("vip");
    expect(screen.getByLabelText("Query 1 SQL")).toHaveValue("SELECT 1");
    await user.click(screen.getByRole("button", { name: "Save and test" }));
    await screen.findByText("The connection test failed");
    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.body).toEqual({
      name: "Warehouse",
      config: { host: "db.example.com", database: "shop", queries: [{ name: "vip", sql: "SELECT 1" }] },
      credential_id: "cred-pg",
    });
  });

  it("says why saving failed and stays on the settings", async () => {
    mockApi({
      ...BASE,
      "POST /api/v1/workspaces/ws1/sources": () => problem(409, "Conflict", "a source with that name already exists"),
    });
    const user = userEvent.setup();
    mount();

    await fillSettings(user);
    await user.click(screen.getByRole("button", { name: "Save and test" }));

    expect(await screen.findByText(/a source with that name already exists/)).toBeInTheDocument();
    expect(screen.getByLabelText("Name")).toHaveValue("CRM");
  });

  it("shows a loading state and an error with a retry for the source types", async () => {
    let attempts = 0;
    mockApi({
      ...BASE,
      "GET /api/v1/source-kinds": () => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json([FAKE_KIND]);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("button", { name: "Choose Fake source" })).toBeInTheDocument();
  });

  it("shows a loading state while the access loads, and an error with a retry when it fails", async () => {
    mockApi({ ...BASE, "GET /api/v1/workspaces/ws1/members": () => new Promise<Response>(() => undefined) });
    const loading = mount();

    expect(await screen.findByText("Checking your access")).toBeInTheDocument();
    loading.unmount();

    let attempts = 0;
    mockApi({
      ...BASE,
      "GET /api/v1/workspaces/ws1/members": () => {
        attempts += 1;
        return attempts === 1
          ? problem(500, "Internal Server Error", "internal error")
          : json(members("admin", OWNER.id));
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("button", { name: "Choose Fake source" })).toBeInTheDocument();
  });

  it("says when no source types are available", async () => {
    mockApi({ ...BASE, "GET /api/v1/source-kinds": () => json([]) });
    mount();

    expect(await screen.findByText("No source types are available on this deployment.")).toBeInTheDocument();
    expect(screen.queryByRole("list", { name: "Source types" })).not.toBeInTheDocument();
  });

  it("shows a loading state while the source types load", async () => {
    mockApi({ ...BASE, "GET /api/v1/source-kinds": () => new Promise<Response>(() => undefined) });
    mount();

    expect(await screen.findByText("Loading source types")).toBeInTheDocument();
  });

  it("tells someone who is not an admin that they cannot add sources", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("member", MEMBER.id)),
      "GET /api/v1/source-kinds": () => json([FAKE_KIND]),
    });
    mount();

    expect(await screen.findByText(/Only workspace admins can add sources/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Choose Fake source" })).not.toBeInTheDocument();
  });
});

describe("the wizard for an HTTP manifest source", () => {
  const MANIFEST = `{"version":"v1","base_url":"https://api.example.test","auth":{"type":"bearer"},"streams":[]}`;
  const HTTP = {
    ...BASE,
    "GET /api/v1/source-kinds": () => json([HTTP_KIND]),
    "POST /api/v1/workspaces/ws1/source-manifests/validate": () =>
      json({ valid: true, issues: [], streams: [{ name: "contacts", fields: [{ name: "id", type: "string" }] }] }),
  };

  it("edits the manifest in the manifest editor and sends it as an object", async () => {
    const calls = mockApi({
      ...HTTP,
      "POST /api/v1/workspaces/ws1/sources": () => json(aSource({ kind: "http" }), 201),
      "POST /api/v1/workspaces/ws1/sources/src-1/test": () => json({ ok: true, error: null }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Choose HTTP API (manifest)" }));
    await user.type(screen.getByLabelText("Name"), "API");
    const area = screen.getByLabelText("Manifest");
    // userEvent treats braces as key descriptors, so the text is set in one go.
    await user.click(area);
    await user.paste(MANIFEST);
    expect(await screen.findByText("Save the source to read a sample with its credential.")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Save and test" }));

    expect(await screen.findByText("The connection to CRM works.")).toBeInTheDocument();
    const created = calls.find((call) => call.method === "POST" && call.path.endsWith("/sources"));
    expect((created?.body as { config: unknown }).config).toEqual({ manifest: JSON.parse(MANIFEST) as unknown });
  });

  it("will not save a manifest that is not a JSON object", async () => {
    mockApi(HTTP);
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Choose HTTP API (manifest)" }));
    await user.type(screen.getByLabelText("Name"), "API");
    await user.click(screen.getByLabelText("Manifest"));
    await user.paste("[1, 2]");

    expect(screen.getByRole("button", { name: "Save and test" })).toBeDisabled();
  });

  it("offers a sample once the source is saved and its settings are opened again", async () => {
    mockApi({
      ...HTTP,
      "POST /api/v1/workspaces/ws1/sources": () => json(aSource({ kind: "http" }), 201),
      "POST /api/v1/workspaces/ws1/sources/src-1/test": () => json({ ok: false, error: "the source refused the credential" }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Choose HTTP API (manifest)" }));
    await user.type(screen.getByLabelText("Name"), "API");
    await user.click(screen.getByLabelText("Manifest"));
    await user.paste(MANIFEST);
    await user.click(screen.getByRole("button", { name: "Save and test" }));
    await user.click(await screen.findByRole("button", { name: "Back to settings" }));

    expect(await screen.findByRole("button", { name: "Test with sample" })).toBeInTheDocument();
  });
});
