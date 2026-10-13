import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, MEMBER, mockApi, OWNER, problem, session } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { SourcesPage } from "./SourcesPage";
import { aSource, FAKE_KIND } from "./testData";

function members(role: "admin" | "member", userId: string) {
  return [{ user_id: userId, email: "someone@example.test", role, added_at: "2026-10-01T09:00:00Z" }];
}

function mount() {
  return renderRoute("/w/ws1/sources", "/w/:ws/sources", <SourcesPage />);
}

function mockAsAdmin(sources: () => Response | Promise<Response>) {
  return mockApi({
    "GET /api/v1/session": () => json(session(OWNER)),
    "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
    "GET /api/v1/source-kinds": () => json([FAKE_KIND]),
    "GET /api/v1/workspaces/ws1/sources": sources,
  });
}

describe("sources", () => {
  it("lists each source with its type, status and a link to its page", async () => {
    mockAsAdmin(() => json([aSource(), aSource({ id: "src-2", name: "Warehouse", enabled: false })]));
    mount();

    const table = await screen.findByRole("table", { name: "Sources of this workspace" });
    const rows = within(table).getAllByRole("row");
    expect(within(rows[1]!).getByRole("link", { name: "CRM" })).toHaveAttribute("href", "/w/ws1/sources/src-1");
    expect(await within(rows[1]!).findByText("Fake source")).toBeInTheDocument();
    expect(within(rows[1]!).getByText("Enabled")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("Disabled")).toBeInTheDocument();
    expect(screen.getByText("2 sources in all.")).toBeInTheDocument();
  });

  it("shows a freshness badge per source, and says when a source has no enabled streams", async () => {
    mockAsAdmin(() =>
      json([
        aSource({ freshness: "fresh" }),
        aSource({ id: "src-2", name: "Warehouse", freshness: "stale" }),
        aSource({ id: "src-3", name: "Billing", freshness: "failing" }),
        aSource({ id: "src-4", name: "Idle", freshness: null }),
      ]),
    );
    mount();

    const table = await screen.findByRole("table", { name: "Sources of this workspace" });
    expect(within(table).getByRole("columnheader", { name: "Freshness" })).toBeInTheDocument();
    const rows = within(table).getAllByRole("row");
    expect(within(rows[1]!).getByText("Fresh")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("Stale")).toBeInTheDocument();
    expect(within(rows[3]!).getByText("Failing")).toBeInTheDocument();
    expect(within(rows[4]!).getByText("No enabled streams")).toBeInTheDocument();
    expect(within(rows[4]!).queryByText("Fresh")).not.toBeInTheDocument();
  });

  it("shows a loading state while the sources load", async () => {
    mockAsAdmin(() => new Promise<Response>(() => undefined));
    mount();

    expect(await screen.findByText("Loading sources")).toBeInTheDocument();
  });

  it("shows an empty state that offers a first source", async () => {
    mockAsAdmin(() => json([]));
    mount();

    expect(await screen.findByText("No sources yet")).toBeInTheDocument();
    expect(screen.getAllByRole("link", { name: "New source" })).toHaveLength(2);
  });

  it("shows a retry when loading fails, and loads again", async () => {
    let attempts = 0;
    mockAsAdmin(() => {
      attempts += 1;
      return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json([aSource()]);
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByText("CRM")).toBeInTheDocument();
    expect(attempts).toBe(2);
  });

  it("is read-only for someone who is not an admin", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(MEMBER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("member", MEMBER.id)),
      "GET /api/v1/source-kinds": () => json([FAKE_KIND]),
      "GET /api/v1/workspaces/ws1/sources": () => json([aSource()]),
    });
    mount();

    expect(await screen.findByText("CRM")).toBeInTheDocument();
    expect(await screen.findByText("You can look at sources but not change them.")).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "New source" })).not.toBeInTheDocument();
  });

  it("still lists the sources when the source types cannot be loaded", async () => {
    mockApi({
      "GET /api/v1/session": () => json(session(OWNER)),
      "GET /api/v1/workspaces/ws1/members": () => json(members("admin", OWNER.id)),
      "GET /api/v1/source-kinds": () => problem(500, "Internal Server Error", "internal error"),
      "GET /api/v1/workspaces/ws1/sources": () => json([aSource()]),
    });
    mount();

    const row = (await screen.findByRole("link", { name: "CRM" })).closest("tr")!;
    expect(within(row).getByText("fake")).toBeInTheDocument();
  });
});
