import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { ImportsPage } from "./ImportsPage";
import { anImport } from "./testData";

const members = (role: "admin" | "member", userId: string) => [
  { user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" },
];

function mount() {
  return renderRoute("/w/ws1/imports", "/w/:ws/imports", <ImportsPage />);
}

const DONE = anImport({
  id: "imp-2",
  name: "Customers April",
  file_name: "april.csv",
  status: "succeeded",
  rows_total: 1200,
  rows_done: 1200,
  rows_applied: 1150,
  rows_unchanged: 0,
  rows_rejected: 50,
});
const RUNNING = anImport({ id: "imp-3", name: "Leads", status: "running", rows_total: 10, rows_done: 4, rows_applied: 4 });

describe("the import history", () => {
  it("lists every import with its status and counters and offers the rejects to download", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/imports": () => json({ imports: [RUNNING, DONE], next_before: null }),
    });
    mount();

    const table = await screen.findByRole("table", { name: "Imports of this workspace" });
    const rows = within(table).getAllByRole("row");
    expect(within(rows[1]!).getByRole("link", { name: "Leads" })).toHaveAttribute("href", "/w/ws1/imports/imp-3");
    expect(within(rows[1]!).getByText("Importing")).toBeInTheDocument();
    expect(within(rows[1]!).getByText("4 / 10")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("Imported")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("1,150")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("april.csv")).toBeInTheDocument();
    expect(within(rows[1]!).queryByRole("link", { name: /Download rejects/ })).not.toBeInTheDocument();
    expect(within(rows[2]!).getByRole("link", { name: "Download rejects of Customers April" })).toHaveAttribute(
      "href",
      "/api/v1/workspaces/ws1/imports/imp-2/rejects.csv",
    );
    expect(screen.getByRole("link", { name: "New import" })).toHaveAttribute("href", "/w/ws1/imports/new");
  });

  it("shows a loading state while the history loads", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/imports": () => new Promise<Response>(() => undefined),
    });
    mount();

    expect(await screen.findByText("Loading imports")).toBeInTheDocument();
  });

  it("shows an empty state that offers a first import", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/imports": () => json({ imports: [], next_before: null }),
    });
    mount();

    expect(await screen.findByText("No imports yet")).toBeInTheDocument();
    expect(screen.getAllByRole("link", { name: "New import" })).toHaveLength(2);
  });

  it("shows a retry when loading fails, and loads again", async () => {
    let attempts = 0;
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/imports": () => {
        attempts += 1;
        return attempts === 1
          ? problem(500, "Internal Server Error", "internal error")
          : json({ imports: [DONE], next_before: null });
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByText("Customers April")).toBeInTheDocument();
    expect(attempts).toBe(2);
  });

  it("is read-only for someone who cannot change anything", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", "someone-else")),
      "GET /api/v1/workspaces/ws1/imports": () => json({ imports: [DONE], next_before: null }),
    });
    mount();

    expect(await screen.findByText("Customers April")).toBeInTheDocument();
    expect(await screen.findByText("You can look at imports but not start one.")).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "New import" })).not.toBeInTheDocument();
  });

  it("lets a plain member start an import", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("member", MEMBER.id)),
      "GET /api/v1/workspaces/ws1/imports": () => json({ imports: [DONE], next_before: null }),
    });
    mount();

    expect(await screen.findByRole("link", { name: "New import" })).toBeInTheDocument();
  });

  it("shows older imports a page at a time", async () => {
    const calls = mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/workspaces/ws1/imports": (request) =>
        new URL(request.url).searchParams.get("before") === "cursor-1"
          ? json({ imports: [DONE], next_before: null })
          : json({ imports: [RUNNING], next_before: "cursor-1" }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Show older imports" }));

    expect(await screen.findByText("Customers April")).toBeInTheDocument();
    expect(screen.getByText("Leads")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Show older imports" })).not.toBeInTheDocument();
    expect(calls.filter((call) => call.path.endsWith("/imports")).map((call) => call.search)).toEqual(["", "?before=cursor-1"]);
  });
});
