import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, OWNER, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { ApiKeysPage } from "./ApiKeysPage";

const CREATED = {
  id: "5d1f0c1e-2b0a-4a55-9d1b-7f3c0a91e0aa",
  name: "Nightly import",
  scopes: ["read"],
  created_at: "2026-10-11T08:00:00Z",
  last_used_at: null,
  revoked_at: null,
  key: "zfk_this-secret-is-shown-once",
};

describe("API keys", () => {
  it("shows the secret once and removes it when the dialog is closed", async () => {
    const listed = { id: CREATED.id, name: CREATED.name, scopes: CREATED.scopes, created_at: CREATED.created_at, last_used_at: null, revoked_at: null };
    let created = false;
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-keys")),
      "GET /api/v1/workspaces/ws1/members": () =>
        json([{ user_id: OWNER.id, email: OWNER.email, role: "admin", added_at: "2026-10-01T09:00:00Z" }]),
      "GET /api/v1/workspaces/ws1/api-keys": () => json(created ? [listed] : []),
      "POST /api/v1/workspaces/ws1/api-keys": () => {
        created = true;
        return json(CREATED, 201);
      },
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/api-keys", "/w/:ws/api-keys", <ApiKeysPage />);

    expect(await screen.findByText("No API keys yet")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Create key" }));

    const form = within(screen.getByRole("dialog", { name: "Create an API key" }));
    await user.type(form.getByLabelText("Name"), "Nightly import");
    await user.click(form.getByRole("button", { name: "Create key" }));

    const secretDialog = await screen.findByRole("dialog", { name: "Copy your new key" });
    expect(within(secretDialog).getByText("zfk_this-secret-is-shown-once")).toBeInTheDocument();
    expect(within(secretDialog).getByText(/You will not see this key again/)).toBeInTheDocument();

    await user.click(within(secretDialog).getByRole("button", { name: "I have copied it" }));

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.queryByText("zfk_this-secret-is-shown-once")).not.toBeInTheDocument();
    expect(await screen.findByText("Nightly import")).toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({ name: "Nightly import", scopes: ["read"] });
    expect(post?.headers.get("x-csrf-token")).toBe("csrf-keys");
  });

  it("keeps focus inside the secret dialog, closes it on Escape and returns focus to the trigger", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () =>
        json([{ user_id: OWNER.id, email: OWNER.email, role: "admin", added_at: "2026-10-01T09:00:00Z" }]),
      "GET /api/v1/workspaces/ws1/api-keys": () => json([]),
      "POST /api/v1/workspaces/ws1/api-keys": () => json(CREATED, 201),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/api-keys", "/w/:ws/api-keys", <ApiKeysPage />);

    const trigger = await screen.findByRole("button", { name: "Create key" });
    await user.click(trigger);
    const form = within(screen.getByRole("dialog", { name: "Create an API key" }));
    await user.type(form.getByLabelText("Name"), "Nightly import");
    await user.click(form.getByRole("button", { name: "Create key" }));

    const secretDialog = await screen.findByRole("dialog", { name: "Copy your new key" });
    expect(secretDialog).toContainElement(document.activeElement as HTMLElement);
    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
  });
});
