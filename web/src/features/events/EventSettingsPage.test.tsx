import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { EventSettingsPage } from "./EventSettingsPage";

const POLICY = {
  unplanned: "draft",
  future_skew_seconds: 300,
  lateness_seconds: 86400,
  redaction: { drop: [], hash: ["email"], truncate_ip: ["ip", "context.ip"] },
};

const URI = "/api/v1/workspaces/ws1/event-policy";

function members(role: "admin" | "editor", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function open() {
  return renderRoute("/w/ws1/events/settings", "/w/:ws/events/settings", <EventSettingsPage />);
}

describe("event settings", () => {
  it("shows the policy that is in force", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      [`GET ${URI}`]: () => json(POLICY),
    });
    open();

    expect(await screen.findByLabelText("When an event has no published schema")).toHaveValue("draft");
    expect(screen.getByLabelText("Future skew in seconds")).toHaveValue(300);
    expect(screen.getByLabelText("Lateness window in seconds")).toHaveValue(86400);
    expect(screen.getByLabelText("Replace with a keyed hash")).toHaveValue("email");
    expect(screen.getByLabelText("Truncate IP addresses")).toHaveValue("ip, context.ip");
    expect(screen.getByRole("button", { name: "Save settings" })).toBeDisabled();
  });

  it("is read-only for a member who is not an admin", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("editor", MEMBER.id)),
      [`GET ${URI}`]: () => json(POLICY),
    });
    open();

    const select = await screen.findByLabelText("When an event has no published schema");
    expect(select).toBeDisabled();
    expect(screen.getByLabelText("Future skew in seconds")).toBeDisabled();
    expect(screen.getByLabelText("Remove")).toBeDisabled();
    expect(screen.queryByRole("button", { name: "Save settings" })).not.toBeInTheDocument();
    expect(screen.getByText(/Only workspace admins and the instance owner can change these settings/)).toBeInTheDocument();
  });

  it("saves the whole policy with parsed paths", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER, "csrf-events")),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      [`GET ${URI}`]: () => json(POLICY),
      [`PUT ${URI}`]: () => json({ ...POLICY, unplanned: "reject" }),
    });
    const user = userEvent.setup();
    open();

    await user.selectOptions(await screen.findByLabelText("When an event has no published schema"), "reject");
    await user.type(screen.getByLabelText("Remove"), "card, context.ssn");
    await user.click(screen.getByRole("button", { name: "Save settings" }));

    await waitFor(() => expect(calls.some((call) => call.method === "PUT")).toBe(true));
    const put = calls.find((call) => call.method === "PUT");
    expect(put?.body).toEqual({
      unplanned: "reject",
      future_skew_seconds: 300,
      lateness_seconds: 86400,
      redaction: { drop: ["card", "context.ssn"], hash: ["email"], truncate_ip: ["ip", "context.ip"] },
    });
    expect(put?.headers.get("x-csrf-token")).toBe("csrf-events");
  });

  it("will not save a blank time limit, and shows the server's reason for a refused policy", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      [`GET ${URI}`]: () => json(POLICY),
      [`PUT ${URI}`]: () => problem(422, "Unprocessable Entity", "future_skew_seconds: must be from 0 to 86400"),
    });
    const user = userEvent.setup();
    open();

    const skew = await screen.findByLabelText("Future skew in seconds");
    await user.clear(skew);
    expect(screen.getByRole("button", { name: "Save settings" })).toBeDisabled();
    await user.type(skew, "90000");
    await user.click(screen.getByRole("button", { name: "Save settings" }));

    expect(await screen.findByText(/must be from 0 to 86400/)).toBeInTheDocument();
  });

  it("shows a retry when the policy cannot be loaded", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      [`GET ${URI}`]: () => problem(500, "Internal Server Error", "internal error"),
    });
    open();

    expect(await screen.findByRole("button", { name: /try again/i })).toBeInTheDocument();
  });
});
