import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { createMemoryRouter, RouterProvider } from "react-router";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { visibleDestinations } from "./navigation";
import { PagePalette } from "./PagePalette";

beforeEach(() => { Element.prototype.scrollIntoView = vi.fn(); });

function Harness({ base }: { base: string | null }) {
  const [open, setOpen] = useState(true);
  return open ? <PagePalette base={base} items={visibleDestinations({ canAdmin: false, isOwner: false })} onClose={() => setOpen(false)} /> : <p>Closed</p>;
}

function palette(base: string | null = "/w/ws1") {
  const router = createMemoryRouter([{ path: "*", element: <Harness base={base} /> }], { initialEntries: ["/w/ws1"] });
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
    expect(screen.getByText("No pages match")).toBeInTheDocument();
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
});
