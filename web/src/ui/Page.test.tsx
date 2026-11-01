import { render, screen, within } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";

import { PageHeader, RelatedPanel } from "./Page";

function header(props: Parameters<typeof PageHeader>[0]) {
  return render(
    <MemoryRouter>
      <PageHeader {...props} />
    </MemoryRouter>,
  );
}

describe("the page header", () => {
  it("draws the breadcrumb, title, status and actions in their regions", () => {
    header({
      title: "Big spenders",
      status: "12,940 people",
      breadcrumb: { group: "Audience", section: "Segments", to: "/w/ws1/segments" },
      primary: <button type="button">Edit rules</button>,
      secondary: <button type="button">Duplicate</button>,
    });

    const crumbs = screen.getByRole("navigation", { name: "Breadcrumb" });
    expect(within(crumbs).getByText("Audience")).toBeInTheDocument();
    expect(within(crumbs).getByRole("link", { name: "Segments" })).toHaveAttribute("href", "/w/ws1/segments");
    expect(screen.getAllByRole("heading", { level: 1 })).toHaveLength(1);
    expect(screen.getByRole("heading", { level: 1, name: "Big spenders" })).toBeInTheDocument();
    expect(screen.getByText("12,940 people")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Edit rules" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Duplicate" })).toBeInTheDocument();
  });

  it("leaves the section as plain text on a list page", () => {
    header({ title: "Segments", breadcrumb: { group: "Audience", section: "Segments" } });

    expect(within(screen.getByRole("navigation", { name: "Breadcrumb" })).queryByRole("link")).not.toBeInTheDocument();
  });

  it("renders as before with only a title, a lede and actions", () => {
    header({ title: "Lists", lede: "Named groups.", actions: <button type="button">New list</button> });

    expect(screen.queryByRole("navigation")).not.toBeInTheDocument();
    expect(screen.getByText("Named groups.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "New list" })).toBeInTheDocument();
  });
});

describe("the related panel", () => {
  it("lists only the groups that have items", () => {
    render(
      <MemoryRouter>
        <RelatedPanel
          groups={[
            { label: "Built from", items: [{ key: "a", label: "city", meta: "field" }] },
            { label: "Used by flows", items: [] },
          ]}
        />
      </MemoryRouter>,
    );

    const panel = screen.getByRole("complementary", { name: "Related" });
    expect(within(panel).getByText("Built from")).toBeInTheDocument();
    expect(within(panel).getByText("city")).toBeInTheDocument();
    expect(within(panel).queryByText("Used by flows")).not.toBeInTheDocument();
  });

  it("is not drawn when every group is empty", () => {
    render(
      <MemoryRouter>
        <RelatedPanel groups={[{ label: "Built from", items: [] }]} />
      </MemoryRouter>,
    );

    expect(screen.queryByRole("complementary")).not.toBeInTheDocument();
  });
});
