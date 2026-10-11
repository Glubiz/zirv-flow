import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { PrivacyNav } from "./PrivacyNav";
import { PrivacyPage } from "./PrivacyPage";

const BASE = "/api/v1/workspaces/ws1/privacy";
const ROUTE = "/w/:ws/privacy";
const PERSON = "0190aaaa-1111-7000-8000-000000000001";

const DONE = {
  id: "e1",
  person_id: PERSON,
  status: "completed",
  requested_by: OWNER.id,
  requested_at: "2026-10-01T09:00:00Z",
  completed_at: "2026-10-01T09:01:00Z",
  trace_id: "a".repeat(32),
};
const FAILED = { ...DONE, id: "e2", status: "failed", completed_at: null, trace_id: "b".repeat(32) };

function exportBody(overrides: Record<string, unknown>) {
  return {
    id: "x1",
    person_id: PERSON,
    status: "ready",
    requested_by: OWNER.id,
    requested_at: "2026-10-01T09:00:00Z",
    ready_at: "2026-10-01T09:01:00Z",
    expires_at: new Date(Date.now() + 3 * 3600 * 1000).toISOString(),
    size_bytes: 1200,
    trace_id: "c".repeat(32),
    ...overrides,
  };
}

function members(role: "admin" | "member", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function adminRoutes(extra: Parameters<typeof mockApi>[0] = {}) {
  return {
    "GET /api/v1/session": () => json(session(OWNER, "csrf-privacy")),
    "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
    [`GET ${BASE}/erasures`]: () => json({ erasures: [], next_before: null }),
    [`GET ${BASE}/exports`]: () => json({ exports: [], next_before: null }),
    [`GET ${BASE}/settings`]: () => json({ orphan_policy: "keep" }),
    ...extra,
  };
}

describe("privacy", () => {
  it("lists erasure requests with their status and shows no personal value", async () => {
    mockApi(adminRoutes({ [`GET ${BASE}/erasures`]: () => json({ erasures: [DONE, FAILED], next_before: null }) }));
    renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);

    const table = await screen.findByRole("table", { name: "Erasure requests" });
    expect(within(table).getByText("Erased")).toBeInTheDocument();
    expect(within(table).getByText("Failed")).toBeInTheDocument();
    expect(within(table).getByText("b".repeat(32))).toBeInTheDocument();
    expect(within(table).queryByText(/@/)).not.toBeInTheDocument();
  });

  it("shows empty states and a retry when a list fails", async () => {
    mockApi(
      adminRoutes({
        [`GET ${BASE}/exports`]: () => problem(500, "Internal Server Error", "internal error"),
      }),
    );
    renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);

    expect(await screen.findByText("No erasure has been requested")).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: /try again/i })).toBeInTheDocument();
  });

  it("asks for a typed confirmation before it requests an erasure, then reloads the list", async () => {
    let listed = 0;
    const calls = mockApi(
      adminRoutes({
        [`GET ${BASE}/erasures`]: () => {
          listed += 1;
          return json({ erasures: listed > 1 ? [{ ...DONE, status: "pending", completed_at: null }] : [], next_before: null });
        },
        [`POST ${BASE}/erasures`]: () => json({ ...DONE, status: "pending", completed_at: null }, 202),
      }),
    );
    const user = userEvent.setup();
    renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);
    const form = await screen.findByRole("form", { name: "Erase a person" });

    await user.type(within(form).getByLabelText("Identifier"), "ada@example.com");
    await user.click(within(form).getByRole("button", { name: "Erase…" }));

    const dialog = await screen.findByRole("dialog", { name: "Erase this person?" });
    expect(within(dialog).getByText(/Their email and phone stay suppressed/)).toBeInTheDocument();
    const confirm = within(dialog).getByRole("button", { name: "Erase the person" });
    expect(confirm).toBeDisabled();
    await user.type(within(dialog).getByLabelText("Type ERASE to confirm"), "erase");
    expect(confirm).toBeDisabled();
    await user.clear(within(dialog).getByLabelText("Type ERASE to confirm"));
    await user.type(within(dialog).getByLabelText("Type ERASE to confirm"), "ERASE");
    await user.click(confirm);

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(await screen.findByText("Waiting to run")).toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST" && call.path === `${BASE}/erasures`);
    expect(post?.body).toEqual({ identifier: { kind: "email", value: "ada@example.com" } });
    expect(post?.headers.get("x-csrf-token")).toBe("csrf-privacy");
  });

  it("requests an erasure by person id and cancels without sending anything", async () => {
    const calls = mockApi(adminRoutes());
    const user = userEvent.setup();
    renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);
    const form = await screen.findByRole("form", { name: "Erase a person" });

    await user.selectOptions(within(form).getByLabelText("Find the person by"), "person");
    await user.type(within(form).getByLabelText("Person id"), PERSON);
    await user.click(within(form).getByRole("button", { name: "Erase…" }));
    await user.click(await screen.findByRole("button", { name: "Cancel" }));

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(calls.some((call) => call.method === "POST")).toBe(false);
  });

  it("shows why a request failed", async () => {
    mockApi(
      adminRoutes({
        [`POST ${BASE}/erasures`]: () => problem(404, "Not Found", "no such resource", "d".repeat(32)),
      }),
    );
    const user = userEvent.setup();
    renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);
    const form = await screen.findByRole("form", { name: "Erase a person" });

    await user.type(within(form).getByLabelText("Identifier"), "nobody@example.com");
    await user.click(within(form).getByRole("button", { name: "Erase…" }));
    await user.type(await screen.findByLabelText("Type ERASE to confirm"), "ERASE");
    await user.click(screen.getByRole("button", { name: "Erase the person" }));

    expect(await screen.findByText("Not Found")).toBeInTheDocument();
    expect(screen.getByText("d".repeat(32))).toBeInTheDocument();
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("lists exports with a countdown and a download link only while they are ready", async () => {
    mockApi(
      adminRoutes({
        [`GET ${BASE}/exports`]: () =>
          json({
            exports: [
              exportBody({}),
              exportBody({ id: "x2", status: "pending", ready_at: null, expires_at: null, size_bytes: null }),
              exportBody({ id: "x3", status: "expired", expires_at: "2026-10-01T10:00:00Z" }),
              exportBody({ id: "x4", status: "failed", ready_at: null, expires_at: null, size_bytes: null }),
            ],
            next_before: null,
          }),
      }),
    );
    renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);

    const table = await screen.findByRole("table", { name: "Exports" });
    expect(within(table).getByText("Ready")).toBeInTheDocument();
    expect(within(table).getByText("Preparing")).toBeInTheDocument();
    expect(within(table).getByText("Expired", { selector: ".pill" })).toBeInTheDocument();
    expect(within(table).getByText("Failed")).toBeInTheDocument();
    expect(within(table).getByText(/^In 3\.0 h/)).toBeInTheDocument();
    const links = within(table).getAllByRole("link", { name: /Download/ });
    expect(links).toHaveLength(1);
    expect(links[0]).toHaveAttribute("href", "/api/v1/workspaces/ws1/privacy/exports/x1/download");
  });

  it("turns a ready export that ran out of time into an expired one", async () => {
    mockApi(
      adminRoutes({
        [`GET ${BASE}/exports`]: () =>
          json({ exports: [exportBody({ expires_at: new Date(Date.now() - 1000).toISOString() })], next_before: null }),
      }),
    );
    renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);

    const table = await screen.findByRole("table", { name: "Exports" });
    expect(within(table).getByText("Expired", { selector: ".pill" })).toBeInTheDocument();
    expect(within(table).queryByRole("link")).not.toBeInTheDocument();
  });

  it("requests an export by identifier and reloads the list", async () => {
    let listed = 0;
    const calls = mockApi(
      adminRoutes({
        [`GET ${BASE}/exports`]: () => {
          listed += 1;
          return json({ exports: listed > 1 ? [exportBody({ status: "pending", ready_at: null, expires_at: null })] : [], next_before: null });
        },
        [`POST ${BASE}/exports`]: () => json(exportBody({ status: "pending" }), 202),
      }),
    );
    const user = userEvent.setup();
    renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);
    const form = await screen.findByRole("form", { name: "Export a person" });

    await user.selectOptions(within(form).getByLabelText("Kind"), "external");
    await user.type(within(form).getByLabelText("Namespace"), "crm");
    await user.type(within(form).getByLabelText("Identifier"), "42");
    await user.click(within(form).getByRole("button", { name: "Request an export" }));

    expect(await screen.findByText("Preparing")).toBeInTheDocument();
    expect(screen.getByText(/Export requested/)).toBeInTheDocument();
    const post = calls.find((call) => call.method === "POST" && call.path === `${BASE}/exports`);
    expect(post?.body).toEqual({ identifier: { kind: "external:crm", value: "42" } });
    expect(post?.headers.get("x-csrf-token")).toBe("csrf-privacy");
  });

  it("saves the orphan policy", async () => {
    const calls = mockApi(
      adminRoutes({
        [`PUT ${BASE}/settings`]: () => json({ orphan_policy: "delete" }),
      }),
    );
    const user = userEvent.setup();
    renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);
    const select = await screen.findByLabelText("Orphan policy");
    const save = screen.getByRole("button", { name: "Save" });
    expect(select).toHaveValue("keep");
    expect(save).toBeDisabled();

    await user.selectOptions(select, "delete");
    expect(screen.getByText(/deleted with everything that refers to them/)).toBeInTheDocument();
    await user.click(save);

    expect(await screen.findByText("Saved.")).toBeInTheDocument();
    const put = calls.find((call) => call.method === "PUT");
    expect(put?.body).toEqual({ orphan_policy: "delete" });
  });

  it("gives a member no privacy tools and asks for none of its data", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("member", MEMBER.id)),
    });
    renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);

    expect(await screen.findByText("Only workspace admins can use this page")).toBeInTheDocument();
    expect(screen.queryByRole("form")).not.toBeInTheDocument();
    expect(calls.some((call) => call.path.includes("/privacy/"))).toBe(false);
  });

  it("shows the menu entry to admins and not to members", async () => {
    mockApi(adminRoutes());
    const first = renderRoute("/w/ws1/privacy", ROUTE, <PrivacyNav workspaceId="ws1" />);
    expect(await screen.findByRole("link", { name: "Privacy" })).toHaveAttribute("href", "/w/ws1/privacy");
    first.unmount();

    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("member", MEMBER.id)),
    });
    renderRoute("/w/ws1/privacy", ROUTE, <PrivacyNav workspaceId="ws1" />);
    await waitFor(() => expect(screen.queryByText("Waiting for the session")).not.toBeInTheDocument());
    expect(screen.queryByRole("link", { name: "Privacy" })).not.toBeInTheDocument();
  });

  describe("polling", () => {
    afterEach(() => {
      vi.useRealTimers();
      Object.defineProperty(document, "hidden", { value: false, configurable: true });
    });

    it("refreshes pending erasures and exports every few seconds and stops when none is pending", async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true });
      let erasures = 0;
      let exports = 0;
      mockApi(
        adminRoutes({
          [`GET ${BASE}/erasures`]: () => {
            erasures += 1;
            return json({ erasures: [erasures > 1 ? DONE : { ...DONE, status: "pending", completed_at: null }], next_before: null });
          },
          [`GET ${BASE}/exports`]: () => {
            exports += 1;
            return json({
              exports: [exportBody(exports > 2 ? {} : { status: "pending", ready_at: null, expires_at: null })],
              next_before: null,
            });
          },
        }),
      );
      renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);
      expect(await screen.findByText("Waiting to run")).toBeInTheDocument();
      expect(await screen.findByText("Preparing")).toBeInTheDocument();

      await vi.advanceTimersByTimeAsync(5000);
      expect(await screen.findByText("Erased")).toBeInTheDocument();
      expect(await screen.findByText("Preparing")).toBeInTheDocument();
      await vi.advanceTimersByTimeAsync(5000);
      expect(await screen.findByText("Ready")).toBeInTheDocument();
      const settled = { erasures, exports };

      await vi.advanceTimersByTimeAsync(30000);

      expect({ erasures, exports }).toEqual(settled);
      expect(settled).toEqual({ erasures: 2, exports: 3 });
    });

    it("keeps polling after one failed refresh, shows the error inline and recovers", async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true });
      let erasures = 0;
      mockApi(
        adminRoutes({
          [`GET ${BASE}/erasures`]: () => {
            erasures += 1;
            if (erasures === 2) return problem(500, "Internal Server Error", "internal error");
            return json({
              erasures: [erasures > 2 ? DONE : { ...DONE, status: "pending", completed_at: null }],
              next_before: null,
            });
          },
        }),
      );
      renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);
      expect(await screen.findByText("Waiting to run")).toBeInTheDocument();

      await vi.advanceTimersByTimeAsync(5000);
      expect(await screen.findByText("Internal Server Error")).toBeInTheDocument();
      expect(screen.getByText("Waiting to run")).toBeInTheDocument();

      await vi.advanceTimersByTimeAsync(5000);
      expect(await screen.findByText("Erased")).toBeInTheDocument();
      expect(screen.queryByText("Internal Server Error")).not.toBeInTheDocument();
      expect(erasures).toBe(3);
    });

    it("skips a beat while a refresh is still in flight, so a slow refresh settles", async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true });
      let erasures = 0;
      mockApi(
        adminRoutes({
          [`GET ${BASE}/erasures`]: async () => {
            erasures += 1;
            if (erasures === 1) {
              return json({ erasures: [{ ...DONE, status: "pending", completed_at: null }], next_before: null });
            }
            // Slower than two beats.
            await new Promise((resolve) => setTimeout(resolve, 12000));
            return json({ erasures: [DONE], next_before: null });
          },
        }),
      );
      renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);
      expect(await screen.findByText("Waiting to run")).toBeInTheDocument();

      // One beat at a time, each flushed, so a render never lags the next beat.
      for (let beat = 0; beat < 4; beat += 1) {
        await act(async () => {
          await vi.advanceTimersByTimeAsync(5000);
        });
      }

      expect(await screen.findByText("Erased")).toBeInTheDocument();
      expect(erasures).toBe(2);
    });

    it("does not poll while the tab is hidden", async () => {
      vi.useFakeTimers({ shouldAdvanceTime: true });
      let erasures = 0;
      mockApi(
        adminRoutes({
          [`GET ${BASE}/erasures`]: () => {
            erasures += 1;
            return json({ erasures: [{ ...DONE, status: "pending", completed_at: null }], next_before: null });
          },
        }),
      );
      renderRoute("/w/ws1/privacy", ROUTE, <PrivacyPage />);
      expect(await screen.findByText("Waiting to run")).toBeInTheDocument();
      Object.defineProperty(document, "hidden", { value: true, configurable: true });

      await vi.advanceTimersByTimeAsync(20000);

      expect(erasures).toBe(1);
    });
  });
});
