import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, OWNER, session, WORKSPACE } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { WorkspaceSwitcher } from "./WorkspaceSwitcher";
import { WorkspacesPage } from "./WorkspacesPage";
import { WorkspacesProvider } from "./WorkspacesProvider";

describe("workspaces", () => {
  it("lets an owner create a workspace and then shows it in the list and the switcher", async () => {
    let created = false;
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-ws")),
      "GET /api/v1/workspaces": () => json(created ? [WORKSPACE] : []),
      "POST /api/v1/workspaces": () => {
        created = true;
        return json(WORKSPACE, 201);
      },
    });
    const user = userEvent.setup();
    renderRoute(
      "/w",
      "/w",
      <WorkspacesProvider>
        <WorkspaceSwitcher />
        <WorkspacesPage />
      </WorkspacesProvider>,
    );

    expect(await screen.findByText("No workspaces yet")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "New workspace" }));
    const dialog = within(screen.getByRole("dialog", { name: "New workspace" }));
    await user.type(dialog.getByLabelText("Name"), "Spring campaigns");
    await user.click(dialog.getByRole("button", { name: "Create workspace" }));

    expect(await screen.findByText("Spring campaigns", { selector: "td" })).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({ name: "Spring campaigns", slug: null });
    expect(post?.headers.get("x-csrf-token")).toBe("csrf-ws");

    await user.click(screen.getByRole("button", { name: "Switch workspace" }));
    expect(screen.getByRole("menuitemradio", { name: "Spring campaigns" })).toBeInTheDocument();
  });
});
