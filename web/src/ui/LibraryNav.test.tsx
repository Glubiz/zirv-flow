import { render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { describe, expect, it } from "vitest";

import { LibraryNav } from "./LibraryNav";

function mount(path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <LibraryNav workspaceId="ws1" />
    </MemoryRouter>,
  );
}

describe("LibraryNav", () => {
  it("links the four library tabs of the workspace", () => {
    mount("/w/ws1/templates");

    const nav = screen.getByRole("navigation", { name: "Template library" });
    const links = Array.from(nav.querySelectorAll("a")).map((link) => [link.textContent, link.getAttribute("href")]);
    expect(links).toEqual([
      ["Templates", "/w/ws1/templates"],
      ["Themes", "/w/ws1/themes"],
      ["Snippets", "/w/ws1/snippets"],
      ["Images", "/w/ws1/images"],
    ]);
  });

  it("marks only the tab of the current page", () => {
    mount("/w/ws1/images");

    expect(screen.getByRole("link", { name: "Images" })).toHaveAttribute("aria-current", "page");
    expect(screen.getByRole("link", { name: "Templates" })).not.toHaveAttribute("aria-current");
  });
});
