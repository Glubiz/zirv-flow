import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createMemoryRouter, RouterProvider, useLocation } from "react-router";
import { describe, expect, it } from "vitest";

import { TabNav, type TabEntry } from "./TabNav";

const TABS: TabEntry[] = [
  { id: "overview", label: "Overview" },
  { id: "rules", label: "Rules" },
  { id: "people", label: "People" },
];

function Probe() {
  const location = useLocation();
  return <p data-testid="search">{location.search}</p>;
}

function mount(url: string, tabs: TabEntry[] = TABS, param: "tab" | "filter" = "tab", label = "Segment sections") {
  const router = createMemoryRouter(
    [
      {
        path: "*",
        element: (
          <>
            <TabNav label={label} tabs={tabs} param={param} />
            <Probe />
          </>
        ),
      },
    ],
    { initialEntries: [url] },
  );
  render(<RouterProvider router={router} />);
  return router;
}

describe("the tab nav", () => {
  it("marks the tab named in the URL and leaves the others unmarked", () => {
    mount("/x?tab=rules");

    expect(screen.getByRole("link", { name: "Rules" })).toHaveAttribute("aria-current", "page");
    expect(screen.getByRole("link", { name: "Overview" })).not.toHaveAttribute("aria-current");
  });

  it("opens the first tab when the URL names none or an unknown one", () => {
    mount("/x?tab=nonsense");

    expect(screen.getByRole("link", { name: "Overview" })).toHaveAttribute("aria-current", "page");
  });

  it("keeps the first tab out of the URL and writes the others in", async () => {
    const user = userEvent.setup();
    mount("/x?tab=people");

    await user.click(screen.getByRole("link", { name: "Rules" }));
    expect(screen.getByTestId("search")).toHaveTextContent("?tab=rules");
    await user.click(screen.getByRole("link", { name: "Overview" }));
    expect(screen.getByTestId("search")).toBeEmptyDOMElement();
  });

  it("walks the tabs with back and forward", async () => {
    const user = userEvent.setup();
    const router = mount("/x");

    await user.click(screen.getByRole("link", { name: "Rules" }));
    await user.click(screen.getByRole("link", { name: "People" }));
    await router.navigate(-1);
    expect(await screen.findByRole("link", { name: "Rules" })).toHaveAttribute("aria-current", "page");
    await router.navigate(-1);
    expect(await screen.findByRole("link", { name: "Overview" })).toHaveAttribute("aria-current", "page");
    await router.navigate(1);
    expect(await screen.findByRole("link", { name: "Rules" })).toHaveAttribute("aria-current", "page");
  });

  it("moves focus with the arrow keys and leaves the nav with Tab", async () => {
    const user = userEvent.setup();
    mount("/x");

    await user.tab();
    expect(screen.getByRole("link", { name: "Overview" })).toHaveFocus();
    await user.keyboard("{ArrowRight}");
    expect(screen.getByRole("link", { name: "Rules" })).toHaveFocus();
    await user.keyboard("{ArrowLeft}{ArrowLeft}");
    expect(screen.getByRole("link", { name: "People" })).toHaveFocus();
    await user.tab();
    expect(screen.getByRole("link", { name: "People" })).not.toHaveFocus();
  });

  it("filters with counts and keeps other parameters", async () => {
    const user = userEvent.setup();
    mount(
      "/x?q=big",
      [
        { id: "all", label: "All", count: 18 },
        { id: "used", label: "Used by a flow", count: 9 },
      ],
      "filter",
      "Filter segments",
    );

    expect(screen.getByRole("navigation", { name: "Filter segments" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "All 18" })).toHaveAttribute("aria-current", "page");
    await user.click(screen.getByRole("link", { name: "Used by a flow 9" }));
    expect(screen.getByTestId("search")).toHaveTextContent("?q=big&filter=used");
  });
});
