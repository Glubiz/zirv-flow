import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { TopicsPage } from "./TopicsPage";

const NEWS = {
  id: "t-news",
  key: "newsletter",
  name: "Newsletter",
  description: "Monthly news",
  channel: "email",
  policy: "opt_out",
  archived: false,
  archived_at: null,
  created_at: "2026-10-01T09:00:00Z",
  updated_at: "2026-10-01T09:00:00Z",
};
const OFFERS = { ...NEWS, id: "t-offers", key: "offers", name: "Offers", description: "", policy: "opt_in" };

function members(role: "admin" | "member", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function mountAsAdmin(topics: () => unknown[]) {
  return mockApi({
    "GET /api/v1/session": () => json(session(OWNER, "csrf-topics")),
    "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
    "GET /api/v1/workspaces/ws1/topics": () => json(topics()),
  });
}

const ROUTE = "/w/:ws/topics";

describe("topics", () => {
  it("lists every topic with what its policy means", async () => {
    mountAsAdmin(() => [NEWS, OFFERS]);
    renderRoute("/w/ws1/topics", ROUTE, <TopicsPage />);

    const table = await screen.findByRole("table", { name: "Topics of this workspace" });
    expect(within(table).getByText("Newsletter")).toBeInTheDocument();
    expect(within(table).getByText("newsletter")).toBeInTheDocument();
    expect(within(table).getByText("Opt-out")).toBeInTheDocument();
    expect(within(table).getByText(/unless they have unsubscribed/)).toBeInTheDocument();
    expect(within(table).getByText("Opt-in")).toBeInTheDocument();
    expect(within(table).getByText(/only after they have subscribed/)).toBeInTheDocument();
  });

  it("shows an empty state, and a retry when loading fails", async () => {
    mountAsAdmin(() => []);
    const first = renderRoute("/w/ws1/topics", ROUTE, <TopicsPage />);
    expect(await screen.findByText("No active topics")).toBeInTheDocument();
    first.unmount();

    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/topics": () => problem(500, "Internal Server Error", "internal error"),
    });
    renderRoute("/w/ws1/topics", ROUTE, <TopicsPage />);
    expect(await screen.findByRole("button", { name: /try again/i })).toBeInTheDocument();
  });

  it("is read-only for a member who is not an admin", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("member", MEMBER.id)),
      "GET /api/v1/workspaces/ws1/topics": () => json([NEWS]),
    });
    renderRoute("/w/ws1/topics", ROUTE, <TopicsPage />);

    expect(await screen.findByText("Newsletter")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add topic" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Edit/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Archive/ })).not.toBeInTheDocument();
    expect(screen.getByText(/Only workspace admins and the instance owner can add or change topics/)).toBeInTheDocument();
  });

  it("adds a topic and explains the chosen policy as it changes", async () => {
    let created = false;
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-topics")),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/topics": () => json(created ? [OFFERS] : []),
      "POST /api/v1/workspaces/ws1/topics": () => {
        created = true;
        return json(OFFERS, 201);
      },
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/topics", ROUTE, <TopicsPage />);

    await user.click(await screen.findByRole("button", { name: "Add topic" }));
    const form = within(screen.getByRole("dialog", { name: "Add a topic" }));
    expect(form.getByRole("button", { name: "Add topic" })).toBeDisabled();
    expect(form.getByText(/unless they have unsubscribed/)).toBeInTheDocument();
    await user.type(form.getByLabelText("Key"), "offers");
    await user.type(form.getByLabelText("Name"), "Offers");
    await user.selectOptions(form.getByLabelText("Without an answer"), "opt_in");
    expect(form.getByText(/only after they have subscribed/)).toBeInTheDocument();
    await user.click(form.getByRole("button", { name: "Add topic" }));

    expect(await screen.findByText("Offers")).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toEqual({ key: "offers", name: "Offers", description: "", channel: "email", policy: "opt_in" });
    expect(post?.headers.get("x-csrf-token")).toBe("csrf-topics");
  });

  it("shows the server's reason when a key is taken and keeps the dialog open", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/topics": () => json([NEWS]),
      "POST /api/v1/workspaces/ws1/topics": () => problem(409, "Conflict", "a topic with that key already exists"),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/topics", ROUTE, <TopicsPage />);

    await user.click(await screen.findByRole("button", { name: "Add topic" }));
    const form = within(screen.getByRole("dialog", { name: "Add a topic" }));
    await user.type(form.getByLabelText("Key"), "newsletter");
    await user.type(form.getByLabelText("Name"), "Again");
    await user.click(form.getByRole("button", { name: "Add topic" }));

    expect(await form.findByText(/a topic with that key already exists/)).toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "Add a topic" })).toBeInTheDocument();
  });

  it("edits a topic and sends only what changed", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-topics")),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/topics": () => json([NEWS]),
      "PATCH /api/v1/workspaces/ws1/topics/t-news": () => json({ ...NEWS, policy: "opt_in" }),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/topics", ROUTE, <TopicsPage />);

    await user.click(await screen.findByRole("button", { name: "Edit Newsletter" }));
    const form = within(screen.getByRole("dialog", { name: "Edit Newsletter" }));
    expect(form.queryByLabelText("Key")).not.toBeInTheDocument();
    expect(form.getByRole("button", { name: "Save changes" })).toBeDisabled();
    await user.selectOptions(form.getByLabelText("Without an answer"), "opt_in");
    await user.click(form.getByRole("button", { name: "Save changes" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    const patch = calls.find((call) => call.method === "PATCH");
    expect(patch?.body).toEqual({ policy: "opt_in" });
    expect(patch?.headers.get("x-csrf-token")).toBe("csrf-topics");
  });

  it("archives after confirmation, restores straight away and can list archived topics", async () => {
    let archived = false;
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-topics")),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/topics": () => json([{ ...NEWS, archived }]),
      "PATCH /api/v1/workspaces/ws1/topics/t-news": async (request) => {
        archived = ((await request.clone().json()) as { archived: boolean }).archived;
        return json({ ...NEWS, archived });
      },
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/topics", ROUTE, <TopicsPage />);

    await user.click(await screen.findByRole("button", { name: "Archive Newsletter" }));
    const confirm = within(screen.getByRole("dialog", { name: "Archive Newsletter?" }));
    await user.click(confirm.getByRole("button", { name: "Archive Newsletter" }));
    await user.click(await screen.findByRole("button", { name: "Restore Newsletter" }));
    expect(await screen.findByRole("button", { name: "Archive Newsletter" })).toBeInTheDocument();
    await user.click(screen.getByLabelText("Show archived topics"));

    await waitFor(() => expect(calls.some((call) => call.search.includes("archived=true"))).toBe(true));
    const patches = calls.filter((call) => call.method === "PATCH").map((call) => call.body);
    expect(patches).toEqual([{ archived: true }, { archived: false }]);
  });

  it("closes a dialog with Escape", async () => {
    mountAsAdmin(() => [NEWS]);
    const user = userEvent.setup();
    renderRoute("/w/ws1/topics", ROUTE, <TopicsPage />);

    await user.click(await screen.findByRole("button", { name: "Add topic" }));
    expect(within(screen.getByRole("dialog")).getByLabelText("Key")).toHaveFocus();
    await user.keyboard("{Escape}");

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add topic" })).toHaveFocus();
  });
});
