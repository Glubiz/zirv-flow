import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { createMemoryRouter, RouterProvider } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { json, mockApi, problem } from "../test/mockApi";
import { visibleDestinations } from "./navigation";
import { PagePalette } from "./PagePalette";

const FLOWS = [
  { id: "f1", name: "Welcome series", state: "published" },
  { id: "f2", name: "Win-back", state: "draft" },
  { id: "f3", name: "Series two", state: "draft" },
];
const SEGMENTS = [{ id: "s1", name: "Series buyers", member_count: 42 }];
const TEMPLATES = [{ id: "t1", name: "Order shipped", current_version: 2 }, { id: "t2", name: "Seriously draft", current_version: null }];
const PERSON = { id: "p1", updated_at: "2026-10-01T09:00:00Z", display: { kind: "email", masked: true, value: "a***@example.test" } };

let calls: ReturnType<typeof mockApi> = [];
let routes: Parameters<typeof mockApi>[0] = {};

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  routes = {
    "GET /api/v1/workspaces/ws1/flows": () => json(FLOWS),
    "GET /api/v1/workspaces/ws1/segments": () => json(SEGMENTS),
    "GET /api/v1/workspaces/ws1/templates": () => json(TEMPLATES),
    "POST /api/v1/workspaces/ws1/people/search": () => json({ people: [PERSON], next_cursor: null }),
  };
  calls = mockApi(routes);
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  vi.useRealTimers();
});

function Harness({ base, people }: { base: string | null; people: boolean }) {
  const [open, setOpen] = useState(true);
  return open ? <PagePalette base={base} workspaceId={base === null ? null : "ws1"} canSearchPeople={people} items={visibleDestinations({ canAdmin: false, isOwner: false })} onClose={() => setOpen(false)} /> : <p>Closed</p>;
}

function OpenerHarness() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button onClick={() => setOpen(true)}>Open</button>
      {open ? <PagePalette base="/w/ws1" workspaceId="ws1" canSearchPeople={false} items={[]} onClose={() => setOpen(false)} /> : null}
    </>
  );
}

function palette(base: string | null = "/w/ws1", people = true) {
  const router = createMemoryRouter([{ path: "*", element: <Harness base={base} people={people} /> }], { initialEntries: ["/w/ws1"] });
  render(<RouterProvider router={router} />);
  return router;
}

describe("page palette", () => {
  it("filters main and settings pages, and navigates with arrows and Enter", async () => {
    const user = userEvent.setup();
    const router = palette();
    const search = screen.getByRole("combobox", { name: "Search pages" });
    expect(search).toHaveFocus();
    await user.type(search, "  EVENTS  ");
    expect(screen.getByRole("option", { name: "Event settings Events" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: /People/ })).not.toBeInTheDocument();
    await user.keyboard("{ArrowDown}{Enter}");
    expect(router.state.location.pathname).toBe("/w/ws1/events/explorer");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("handles an empty result and closes on Escape", async () => {
    const user = userEvent.setup();
    const router = palette();
    await user.type(screen.getByRole("combobox"), "no-such-page");
    expect(await screen.findByText("No pages match")).toBeInTheDocument();
    await user.keyboard("{ArrowUp}{Enter}");
    expect(router.state.location.pathname).toBe("/w/ws1");
    await user.keyboard("{Escape}");
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("excludes restricted and unavailable destinations", () => {
    palette(null);
    expect(screen.getByRole("option", { name: "Health System" })).toBeInTheDocument();
    for (const name of [/Jobs/, /Privacy/, /Audit log/, /Destinations/, /Sending domains/, /Home/, /People/]) {
      expect(screen.queryByRole("option", { name })).not.toBeInTheDocument();
    }
  });

  it("wraps ArrowUp and allows pointer selection", async () => {
    const user = userEvent.setup();
    const router = palette();
    await user.keyboard("{ArrowUp}");
    expect(screen.getByRole("option", { name: "Workspaces" })).toHaveAttribute("aria-selected", "true");
    await user.click(screen.getByRole("option", { name: "Members and roles Workspace" }));
    expect(router.state.location.pathname).toBe("/w/ws1/members");
  });

  it("shows only Pages for an empty query and loads each list once", async () => {
    const user = userEvent.setup();
    palette();
    expect(screen.getByRole("group", { name: "Pages" })).toBeInTheDocument();
    for (const name of ["Flows", "Segments", "Templates", "People"]) expect(screen.queryByRole("group", { name })).not.toBeInTheDocument();
    await user.type(screen.getByRole("combobox"), "series");
    await screen.findByRole("option", { name: "Welcome series, flow, live" });
    await user.type(screen.getByRole("combobox"), " b");
    expect(calls.filter((call) => call.method === "GET").map((call) => call.path).sort()).toEqual([
      "/api/v1/workspaces/ws1/flows", "/api/v1/workspaces/ws1/segments", "/api/v1/workspaces/ws1/templates",
    ]);
  });

  it("groups matches under headings, prefix first, at most 5 per group", async () => {
    routes["GET /api/v1/workspaces/ws1/flows"] = () => json([
      ...Array.from({ length: 7 }, (_, n) => ({ id: `x${n}`, name: `Big flow ${n}`, state: "draft" })),
      { id: "p", name: "flow first", state: "published" },
    ]);
    const user = userEvent.setup();
    palette();
    await user.type(screen.getByRole("combobox"), "flow");
    const flows = await screen.findByRole("group", { name: "Flows" });
    const options = within(flows).getAllByRole("option");
    expect(options).toHaveLength(5);
    expect(options[0]).toHaveAccessibleName("flow first, flow, live");
  });

  it("shows segment and template states in their groups", async () => {
    const user = userEvent.setup();
    palette();
    await user.type(screen.getByRole("combobox"), "serie");
    expect(await screen.findByRole("option", { name: "Series buyers, segment, 42" })).toBeInTheDocument();
    await user.clear(screen.getByRole("combobox"));
    await user.type(screen.getByRole("combobox"), "serio");
    expect(await screen.findByRole("option", { name: "Seriously draft, template, draft" })).toBeInTheDocument();
  });

  it("moves across groups with arrows, Home and End and opens with Enter", async () => {
    const user = userEvent.setup();
    const router = palette();
    await user.type(screen.getByRole("combobox"), "series");
    await screen.findByRole("option", { name: "Series buyers, segment, 42" });
    const options = () => screen.getAllByRole("option");
    await user.keyboard("{End}");
    expect(options().at(-1)).toHaveAttribute("aria-selected", "true");
    await user.keyboard("{Home}{ArrowDown}");
    expect(options()[1]).toHaveAttribute("aria-selected", "true");
    await user.keyboard("{End}{ArrowUp}{Enter}");
    expect(router.state.location.pathname).toMatch(/^\/w\/ws1\/(flows|segments|templates)\//);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("opens a result in a new tab with Ctrl-Enter", async () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    const user = userEvent.setup();
    palette();
    await user.type(screen.getByRole("combobox"), "welcome");
    await screen.findByRole("option", { name: "Welcome series, flow, live" });
    await user.keyboard("{Control>}{Enter}{/Control}");
    expect(open).toHaveBeenCalledWith("/w/ws1/flows/f1", "_blank", "noopener");
  });

  it("returns focus to the opener on Escape", async () => {
    const user = userEvent.setup();
    const router = createMemoryRouter([{ path: "*", element: <OpenerHarness /> }], { initialEntries: ["/w/ws1"] });
    render(<RouterProvider router={router} />);
    const opener = screen.getByRole("button", { name: "Open" });
    await user.click(opener);
    await user.keyboard("{Escape}");
    expect(opener).toHaveFocus();
  });

  it("debounces object matching by 200 ms", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    palette();
    await act(() => vi.advanceTimersByTimeAsync(50));
    await user.type(screen.getByRole("combobox"), "welcome");
    expect(screen.queryByRole("option", { name: /Welcome series/ })).not.toBeInTheDocument();
    expect(screen.getByText("Searching")).toBeInTheDocument();
    await act(() => vi.advanceTimersByTimeAsync(250));
    expect(screen.getByRole("option", { name: "Welcome series, flow, live" })).toBeInTheDocument();
  });

  it("drops a people response that arrives after the query changed", async () => {
    let release: (() => void) | undefined;
    routes["POST /api/v1/workspaces/ws1/people/search"] = () => new Promise((resolve) => { release = () => resolve(json({ people: [PERSON], next_cursor: null })); });
    const user = userEvent.setup();
    palette();
    await user.type(screen.getByRole("combobox"), "ann@example.test");
    await user.keyboard("{Enter}");
    await user.type(screen.getByRole("combobox"), "x");
    await act(async () => { release?.(); await Promise.resolve(); });
    expect(screen.queryByRole("option", { name: "a***@example.test, person" })).not.toBeInTheDocument();
    expect(screen.getByRole("option", { name: "Find person ann@example.testx" })).toBeInTheDocument();
  });

  it("searches people only on Enter of the explicit row and shows the value as returned", async () => {
    const user = userEvent.setup();
    const router = palette();
    await user.type(screen.getByRole("combobox"), "ann@example.test");
    const find = await screen.findByRole("option", { name: "Find person ann@example.test" });
    expect(calls.filter((call) => call.method === "POST")).toHaveLength(0);
    expect(find).toHaveAttribute("aria-selected", "true");
    await user.keyboard("{Enter}");
    const person = await screen.findByRole("option", { name: "a***@example.test, person" });
    const post = calls.find((call) => call.method === "POST");
    expect(post?.body).toMatchObject({ identifier: { kind: "email", value: "ann@example.test" } });
    expect(post?.search).toBe("");
    await user.click(person);
    expect(router.state.location.pathname).toBe("/w/ws1/people/p1");
    expect(router.state.location.search).toBe("");
  });

  it("hides the People group without permission and keeps other groups when one list fails", async () => {
    routes["GET /api/v1/workspaces/ws1/flows"] = () => problem(500, "Boom", "failed");
    const user = userEvent.setup();
    palette("/w/ws1", false);
    await user.type(screen.getByRole("combobox"), "ann@example.test");
    expect(await screen.findByText("Could not search flows")).toBeInTheDocument();
    expect(screen.queryByRole("group", { name: "People" })).not.toBeInTheDocument();
    await user.clear(screen.getByRole("combobox"));
    await user.type(screen.getByRole("combobox"), "serie");
    expect(await screen.findByRole("option", { name: "Series buyers, segment, 42" })).toBeInTheDocument();
  });
});
