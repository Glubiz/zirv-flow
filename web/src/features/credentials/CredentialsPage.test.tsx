import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { CredentialsPage } from "./CredentialsPage";
import type { Credential, CredentialKind } from "./labels";

const BASIC: CredentialKind = {
  id: "basic",
  label: "Username and password",
  schema: {
    fields: [
      { key: "username", label: "Username", type: "string", required: true, secret: false, options: [] },
      { key: "password", label: "Password", type: "string", required: true, secret: true, options: [] },
    ],
  },
};

const SMTP: CredentialKind = {
  id: "smtp",
  label: "SMTP server",
  schema: {
    fields: [
      { key: "host", label: "Host", type: "string", required: true, secret: false, options: [] },
      { key: "port", label: "Port", type: "integer", required: true, secret: false, options: [] },
      { key: "tls", label: "TLS mode", type: "enum", required: true, secret: false, options: ["starttls", "tls", "none"] },
      { key: "password", label: "Password", type: "string", required: true, secret: true, options: [] },
    ],
  },
};

const BEARER: CredentialKind = {
  id: "bearer_token",
  label: "Bearer token",
  schema: { fields: [{ key: "token", label: "Token", type: "string", required: true, secret: true, options: [] }] },
};

const TLS_CLIENT: CredentialKind = {
  id: "tls_client",
  label: "TLS client certificate",
  schema: {
    fields: [
      { key: "tls_client_cert", label: "Client certificate (PEM)", type: "string", required: true, secret: false, options: [] },
      { key: "tls_client_key", label: "Client private key (PEM)", type: "string", required: true, secret: true, options: [] },
    ],
  },
};

const SIGNING: CredentialKind = {
  id: "signing_secret",
  label: "Signing secret",
  schema: {
    fields: [
      { key: "signing_secret", label: "Signing secret", type: "string", required: true, secret: true, options: [] },
      { key: "previous_signing_secret", label: "Previous signing secret", type: "string", required: false, secret: true, options: [] },
    ],
  },
};

function aCredential(change: Partial<Credential> = {}): Credential {
  return {
    id: "cred-1",
    key: "mail",
    name: "Mail relay",
    kind: "smtp",
    schema_version: 1,
    schema: SMTP.schema,
    fields: { host: "mail.example.test", port: 587, tls: "starttls" },
    hints: { password: "wxyz" },
    fingerprint: "ab".repeat(32),
    same_secret_as: [],
    used_by: [{ kind: "destination", id: "dest-1", name: "Transactional mail" }],
    rotated_at: "2026-10-20T08:00:00Z",
    last_tested_at: null,
    last_test_outcome: null,
    archived: false,
    created_by: null,
    created_at: "2026-10-01T08:00:00Z",
    updated_at: "2026-10-01T08:00:00Z",
    ...change,
  };
}

const ADMIN = {
  "GET /api/v1/session": () => json(session(OWNER, "csrf-credentials")),
  "GET /api/v1/workspaces/ws1/members": () =>
    json([{ user_id: OWNER.id, email: OWNER.email, role: "admin", added_at: "2026-10-01T09:00:00Z" }]),
  "GET /api/v1/workspaces/ws1/credential-kinds": () => json([BASIC, SMTP]),
};

function mount() {
  return renderRoute("/w/ws1/credentials", "/w/:ws/credentials", <CredentialsPage />);
}

describe("credentials", () => {
  it("offers the bearer token, TLS client and signing secret kinds with masked secrets and an optional previous secret", async () => {
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/credential-kinds": () => json([BASIC, SMTP, BEARER, TLS_CLIENT, SIGNING]),
      "GET /api/v1/workspaces/ws1/credentials": () => json([]),
    });
    const user = userEvent.setup();
    mount();

    expect(await screen.findByText("No credentials yet")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "New credential" }));
    const dialog = within(screen.getByRole("dialog", { name: "New credential" }));
    const kind = await dialog.findByLabelText("Kind");
    expect(within(kind).getAllByRole("option").map((option) => option.textContent)).toEqual(
      expect.arrayContaining(["Bearer token", "TLS client certificate", "Signing secret"]),
    );

    await user.selectOptions(kind, "bearer_token");
    expect(dialog.getByLabelText("Token")).toHaveAttribute("type", "password");

    await user.selectOptions(dialog.getByLabelText("Kind"), "tls_client");
    expect(dialog.getByLabelText("Client certificate (PEM)")).toHaveAttribute("type", "text");
    expect(dialog.getByLabelText("Client private key (PEM)")).toHaveAttribute("type", "password");

    await user.selectOptions(dialog.getByLabelText("Kind"), "signing_secret");
    expect(dialog.getByLabelText("Signing secret")).toHaveAttribute("type", "password");
    expect(dialog.getByLabelText("Previous signing secret (optional)")).toHaveAttribute("type", "password");
  });

  it("lists name, kind, hints, used-by count, last test and rotated date, and never a value", async () => {
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/credentials": () =>
        json([
          aCredential({ last_tested_at: "2026-10-21T08:00:00Z", last_test_outcome: "failed" }),
          aCredential({
            id: "cred-2",
            name: "Shared twin",
            kind: "basic",
            hints: { password: "" },
            used_by: [],
            same_secret_as: ["cred-1"],
          }),
        ]),
    });
    mount();

    const table = await screen.findByRole("table", { name: "Credentials of this workspace" });

    const mail = within(within(table).getByRole("row", { name: /Mail relay/ }));
    expect(mail.getByText("SMTP server")).toBeInTheDocument();
    expect(mail.getByText("password ••••wxyz")).toBeInTheDocument();
    expect(mail.getByText("1")).toBeInTheDocument();
    expect(mail.getByText("Failed")).toBeInTheDocument();
    const twin = within(within(table).getByRole("row", { name: /Shared twin/ }));
    expect(twin.getByText("password ••••••••")).toBeInTheDocument();
    expect(twin.getByText("Not tested")).toBeInTheDocument();
    expect(twin.getByText("Same secret as another credential")).toBeInTheDocument();
  });

  it("creates a credential from the schema of the chosen kind and never fills the secret in", async () => {
    let created = false;
    const calls = mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/credentials": () => json(created ? [aCredential()] : []),
      "POST /api/v1/workspaces/ws1/credentials": () => {
        created = true;
        return json(aCredential(), 201);
      },
    });
    const user = userEvent.setup();
    mount();

    expect(await screen.findByText("No credentials yet")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "New credential" }));
    const dialog = within(screen.getByRole("dialog", { name: "New credential" }));
    expect(dialog.getByRole("button", { name: "Create credential" })).toBeDisabled();
    await user.selectOptions(await dialog.findByLabelText("Kind"), "smtp");

    expect(dialog.getByLabelText("Host")).toBeInTheDocument();
    expect(dialog.getByLabelText("Port")).toHaveAttribute("type", "number");
    expect(dialog.getByLabelText("TLS mode").tagName).toBe("SELECT");
    const password = dialog.getByLabelText("Password");
    expect(password).toHaveAttribute("type", "password");
    expect(password).toHaveValue("");
    await user.type(dialog.getByLabelText("Name"), "Mail relay");
    await user.type(dialog.getByLabelText("Host"), "mail.example.test");
    await user.type(dialog.getByLabelText("Port"), "587");
    await user.selectOptions(dialog.getByLabelText("TLS mode"), "starttls");
    expect(dialog.getByRole("button", { name: "Create credential" })).toBeDisabled();
    await user.type(password, "a-long-secret-value");
    await user.click(dialog.getByRole("button", { name: "Create credential" }));

    expect(await screen.findByRole("table", { name: "Credentials of this workspace" })).toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({
      kind: "smtp",
      name: "Mail relay",
      fields: { host: "mail.example.test", port: 587, tls: "starttls" },
      secret: { password: "a-long-secret-value" },
    });
    expect(post?.headers.get("x-csrf-token")).toBe("csrf-credentials");
    expect(screen.queryByText("a-long-secret-value")).not.toBeInTheDocument();
  });

  it("edits the name and the non-secret values with the stored ones filled in and no secret input", async () => {
    const calls = mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/credentials": () => json([aCredential()]),
      "PATCH /api/v1/workspaces/ws1/credentials/cred-1": () => json(aCredential({ name: "Mail relay 2" })),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Edit Mail relay" }));
    const dialog = within(screen.getByRole("dialog", { name: "Edit Mail relay" }));

    expect(dialog.getByLabelText("Host")).toHaveValue("mail.example.test");
    expect(dialog.getByLabelText("Port")).toHaveValue(587);
    expect(dialog.queryByLabelText("Password")).not.toBeInTheDocument();
    await user.clear(dialog.getByLabelText("Name"));
    await user.type(dialog.getByLabelText("Name"), "Mail relay 2");
    await user.click(dialog.getByRole("button", { name: "Save changes" }));

    await screen.findByRole("table", { name: "Credentials of this workspace" });
    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.body).toEqual({
      name: "Mail relay 2",
      fields: { host: "mail.example.test", port: 587, tls: "starttls" },
    });
  });

  it("rotates with every secret input empty and says how many users follow", async () => {
    const calls = mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/credentials": () => json([aCredential()]),
      "PUT /api/v1/workspaces/ws1/credentials/cred-1/secret": () => json(aCredential()),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Rotate Mail relay" }));
    const dialog = within(screen.getByRole("dialog", { name: "Rotate Mail relay" }));

    const password = dialog.getByLabelText("Password");
    expect(password).toHaveValue("");
    expect(password).toHaveAttribute("type", "password");
    expect(dialog.queryByLabelText("Host")).not.toBeInTheDocument();
    expect(dialog.getByText(/1 source or destination uses this credential/)).toBeInTheDocument();
    expect(dialog.getByRole("button", { name: "Rotate secret" })).toBeDisabled();
    await user.type(password, "the-new-secret-value");
    await user.click(dialog.getByRole("button", { name: "Rotate secret" }));

    await screen.findByRole("table", { name: "Credentials of this workspace" });
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const put = calls.find((call) => call.method === "PUT");
    expect(put?.body).toEqual({ secret: { password: "the-new-secret-value" } });
    expect(put?.headers.get("x-csrf-token")).toBe("csrf-credentials");
  });

  it("tests with a chosen user of the credential and shows a fixed failure", async () => {
    const calls = mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/credentials": () =>
        json([
          aCredential({
            used_by: [
              { kind: "destination", id: "dest-1", name: "Transactional mail" },
              { kind: "source", id: "src-1", name: "CRM" },
            ],
          }),
        ]),
      "POST /api/v1/workspaces/ws1/credentials/cred-1/test": () =>
        json({ ok: false, message: "the source refused the credential", code: "credential_refused" }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Test Mail relay" }));
    const dialog = within(screen.getByRole("dialog", { name: "Test Mail relay" }));
    await user.selectOptions(dialog.getByLabelText("Test with"), "source:src-1");
    await user.click(dialog.getByRole("button", { name: "Run test" }));

    expect(await dialog.findByText("The check failed")).toBeInTheDocument();
    expect(dialog.getByText("the source refused the credential")).toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({ source_id: "src-1" });
  });

  it("says there is nothing to test with when nothing uses the credential", async () => {
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/credentials": () => json([aCredential({ used_by: [] })]),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Test Mail relay" }));

    const dialog = within(screen.getByRole("dialog", { name: "Test Mail relay" }));
    expect(dialog.getByText(/Nothing uses this credential yet/)).toBeInTheDocument();
    expect(dialog.queryByRole("button", { name: "Run test" })).not.toBeInTheDocument();
  });

  it("archives an unused credential", async () => {
    let archived = false;
    const calls = mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/credentials": () => json(archived ? [] : [aCredential({ used_by: [] })]),
      "DELETE /api/v1/workspaces/ws1/credentials/cred-1": () => {
        archived = true;
        return new Response(null, { status: 204 });
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Archive Mail relay" }));
    await user.click(within(screen.getByRole("dialog", { name: "Archive Mail relay?" })).getByRole("button", { name: "Archive Mail relay" }));

    expect(await screen.findByText("No credentials yet")).toBeInTheDocument();
    expect(calls.some((call) => call.method === "DELETE")).toBe(true);
  });

  it("lists what still uses a credential when archiving is refused", async () => {
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/credentials": () => json([aCredential()]),
      "GET /api/v1/workspaces/ws1/credentials/cred-1": () =>
        json(
          aCredential({
            used_by: [
              { kind: "destination", id: "dest-1", name: "Transactional mail" },
              { kind: "source", id: "src-1", name: "CRM" },
            ],
          }),
        ),
      "DELETE /api/v1/workspaces/ws1/credentials/cred-1": () =>
        problem(409, "Conflict", "this credential is used by a source or destination", undefined, "credential_in_use"),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Archive Mail relay" }));
    const dialog = within(screen.getByRole("dialog", { name: "Archive Mail relay?" }));
    await user.click(dialog.getByRole("button", { name: "Archive Mail relay" }));

    expect(await dialog.findByText("This credential is still used, so it was not archived.")).toBeInTheDocument();
    const used = within(dialog.getByRole("list", { name: "Used by" }));
    expect(used.getByText("Transactional mail")).toBeInTheDocument();
    expect(used.getByText("CRM")).toBeInTheDocument();
    expect(screen.getByRole("table", { name: "Credentials of this workspace" })).toBeInTheDocument();
  });

  it("shows a loading state, and an error with a retry", async () => {
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/credentials": () => new Promise<Response>(() => undefined),
    });
    const loading = mount();
    expect(await screen.findByText("Loading credentials")).toBeInTheDocument();
    loading.unmount();

    let attempts = 0;
    mockApi({
      ...ADMIN,
      "GET /api/v1/workspaces/ws1/credentials": () => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json([aCredential()]);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByRole("table", { name: "Credentials of this workspace" })).toBeInTheDocument();
  });

  it("tells someone who is not an admin that they cannot manage credentials", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () =>
        json([{ user_id: MEMBER.id, email: MEMBER.email, role: "editor", added_at: "2026-10-01T09:00:00Z" }]),
    });
    renderRoute("/w/ws1/credentials", "/w/:ws/credentials", <CredentialsPage />);

    expect(await screen.findByText("You cannot manage credentials here")).toBeInTheDocument();
  });
});
