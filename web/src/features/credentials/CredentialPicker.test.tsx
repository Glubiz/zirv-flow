import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { describe, expect, it } from "vitest";

import { json, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { SessionProvider } from "../session";
import { CredentialPicker } from "./CredentialPicker";
import type { FieldHint } from "./labels";

const FIELDS: FieldHint[] = [
  { key: "token", label: "Token", required: true, secret: true },
  { key: "region", label: "Region", required: false, secret: false },
];

function aCredential(id: string, name: string, names: string[]) {
  return {
    id,
    key: id,
    name,
    kind: "custom",
    schema_version: 1,
    schema: { fields: names.map((key) => ({ key, label: key, type: "string", required: true, secret: true, options: [] })) },
    fields: {},
    hints: Object.fromEntries(names.map((key) => [key, ""])),
    fingerprint: null,
    same_secret_as: [],
    used_by: [],
    rotated_at: "2026-10-01T09:00:00Z",
    last_tested_at: null,
    last_test_outcome: null,
    archived: false,
    created_by: null,
    created_at: "2026-10-01T09:00:00Z",
    updated_at: "2026-10-01T09:00:00Z",
  };
}

function Harness() {
  const [value, setValue] = useState("");
  return (
    <SessionProvider>
      <CredentialPicker workspaceId="ws1" fields={FIELDS} value={value} onChange={setValue} purpose="CRM" />
      <p>Chosen: {value || "none"}</p>
    </SessionProvider>
  );
}

const BASE = {
  "GET /api/v1/session": () => json(session(OWNER, "csrf-picker")),
};

describe("the credential picker", () => {
  it("offers only credentials that hold the required fields and reports the choice", async () => {
    mockApi({
      ...BASE,
      "GET /api/v1/workspaces/ws1/credentials": () =>
        json([aCredential("c1", "CRM token", ["token"]), aCredential("c2", "Login", ["username", "password"])]),
    });
    const user = userEvent.setup();
    render(<Harness />);

    const select = await screen.findByLabelText("Credential");

    expect(within(select).getAllByRole("option").map((option) => option.textContent)).toEqual([
      "Choose a credential",
      "CRM token (Custom)",
    ]);
    await user.selectOptions(select, "c1");
    expect(screen.getByText("Chosen: c1")).toBeInTheDocument();
  });

  it("creates a credential inline from the type's own fields, then chooses it", async () => {
    let made = false;
    const calls = mockApi({
      ...BASE,
      "GET /api/v1/workspaces/ws1/credentials": () => json(made ? [aCredential("new-1", "CRM credential", ["token"])] : []),
      "POST /api/v1/workspaces/ws1/credentials": () => {
        made = true;
        return json(aCredential("new-1", "CRM credential", ["token"]), 201);
      },
    });
    const user = userEvent.setup();
    render(<Harness />);

    expect(await screen.findByText(/No credential holds the fields this type needs yet/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "New credential" }));
    const dialog = within(screen.getByRole("dialog", { name: "New credential for CRM" }));
    expect(dialog.getByLabelText("Name")).toHaveValue("CRM credential");
    expect(dialog.getByLabelText("Token")).toHaveAttribute("type", "password");
    expect(dialog.getByLabelText("Token")).toHaveValue("");
    expect(dialog.queryByLabelText("Kind")).not.toBeInTheDocument();
    await user.type(dialog.getByLabelText("Token"), "a-secret-token-value");
    await user.click(dialog.getByRole("button", { name: "Create credential" }));

    expect(await screen.findByText("Chosen: new-1")).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({
      kind: "custom",
      name: "CRM credential",
      schema: {
        fields: [
          { key: "token", label: "Token", type: "string", required: true, secret: true, options: [] },
          { key: "region", label: "Region", type: "string", required: false, secret: false, options: [] },
        ],
      },
      fields: {},
      secret: { token: "a-secret-token-value" },
    });
  });

  it("shows a loading state and an error with a retry", async () => {
    mockApi({
      ...BASE,
      "GET /api/v1/workspaces/ws1/credentials": () => new Promise<Response>(() => undefined),
    });
    const loading = render(<Harness />);
    expect(await screen.findByText("Loading credentials")).toBeInTheDocument();
    loading.unmount();

    let attempts = 0;
    mockApi({
      ...BASE,
      "GET /api/v1/workspaces/ws1/credentials": () => {
        attempts += 1;
        return attempts === 1
          ? problem(500, "Internal Server Error", "internal error")
          : json([aCredential("c1", "CRM token", ["token"])]);
      },
    });
    const user = userEvent.setup();
    render(<Harness />);

    expect(await screen.findByText("Could not load the credentials")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Try again" }));

    expect(await screen.findByLabelText("Credential")).toBeInTheDocument();
  });
});
