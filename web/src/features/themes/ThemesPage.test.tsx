import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { listItem, themeRoutes, tokens, usedBy, templateUse, WS } from "../../test/themesApi";
import { ThemesPage } from "./ThemesPage";

function mount() {
  return renderRoute("/w/ws1/themes", "/w/:ws/themes", <ThemesPage />);
}

function refusal(status: number, body: Record<string, unknown>) {
  return new Response(JSON.stringify({ type: "about:blank", title: "Refused", status, detail: "Refused.", ...body }), {
    status,
    headers: { "content-type": "application/problem+json" },
  });
}

const DEFAULT_ID = "77777777-0000-4000-8000-0000000000d1";
const OTHER_ID = "77777777-0000-4000-8000-0000000000d2";

function twoThemes() {
  return [
    listItem({ id: DEFAULT_ID, name: "Warm", current_version: 3, is_default: true }, 14, tokens({ background: "#fbf7f2", primary: "#7a3e1d", font: "georgia", button_radius: 40 })),
    listItem({ id: OTHER_ID, name: "Cool" }, 0, tokens({ background: "#e4ece6", button: "#0b5cff", button_text: "#fefefe" })),
  ];
}

describe("the theme list", () => {
  it("shows a card per theme with the default badge, the draft pill and the version line", async () => {
    mockApi(themeRoutes({ list: twoThemes() }));
    mount();

    const grid = await screen.findByRole("list", { name: "Themes of this workspace" });
    const cards = within(grid).getAllByRole("listitem");
    expect(cards).toHaveLength(2);
    expect(within(cards[0]!).getByRole("link", { name: "Warm" })).toHaveAttribute("href", `/w/ws1/themes/${DEFAULT_ID}`);
    expect(within(cards[0]!).getByText("Default")).toBeInTheDocument();
    expect(within(cards[0]!).getByText("v3 · 14 templates")).toBeInTheDocument();
    expect(within(cards[1]!).getByText("Draft")).toBeInTheDocument();
    expect(within(cards[1]!).getByText("Not published")).toBeInTheDocument();
    expect(within(cards[1]!).queryByText("Default")).not.toBeInTheDocument();
  });

  it("draws each thumbnail from the tokens", async () => {
    mockApi(themeRoutes({ list: twoThemes() }));
    mount();

    const warm = await screen.findByLabelText("Sample of Warm");
    const page = warm.firstElementChild as HTMLElement;
    expect(page.style.background).toBe("rgb(251, 247, 242)");
    expect(page.style.fontFamily).toContain("Georgia");
    const heading = within(warm).getByText("A heading in this theme");
    expect(heading.style.color).toBe("rgb(122, 62, 29)");
    expect(within(warm).getByText("Button").style.borderRadius).toBe("40px");
    const cool = screen.getByLabelText("Sample of Cool");
    expect(within(cool).getByText("Button").style.background).toBe("rgb(11, 92, 255)");
    expect(within(cool).getByText("Button").style.color).toBe("rgb(254, 254, 254)");
  });

  it("shows an empty state, and an error with a retry", async () => {
    let attempts = 0;
    mockApi({
      ...themeRoutes(),
      [`GET ${WS}/themes`]: () => {
        attempts += 1;
        return attempts === 1 ? new Response("{}", { status: 500, headers: { "content-type": "application/json" } }) : json([]);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));
    expect(await screen.findByText("No themes yet")).toBeInTheDocument();
  });

  it("disables Archive on the default theme and says why", async () => {
    mockApi(themeRoutes({ list: twoThemes() }));
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "More actions for Warm" }));

    const archive = screen.getByRole("menuitem", { name: /Archive/ });
    expect(archive).toHaveAttribute("aria-disabled", "true");
    expect(archive).toHaveTextContent("The default theme cannot be archived");
    expect(screen.queryByRole("menuitem", { name: "Set as default" })).not.toBeInTheDocument();
  });

  it("names how many templates follow the default before setting a new one", async () => {
    const calls = mockApi({
      ...themeRoutes({ list: twoThemes() }),
      [`GET ${WS}/themes/${DEFAULT_ID}/where-used`]: () => json(usedBy({ default_templates: [1, 2, 3].map(templateUse) })),
      [`PUT ${WS}/themes/default`]: () => json(listItem({ id: OTHER_ID, name: "Cool", is_default: true })),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "More actions for Cool" }));
    await user.click(screen.getByRole("menuitem", { name: "Set as default" }));

    const dialog = await screen.findByRole("dialog", { name: "Make Cool the default?" });
    expect(await within(dialog).findByText(/3 templates follow the default today/)).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Make default" }));

    await within(document.body).findByRole("list", { name: "Themes of this workspace" });
    expect(calls.find((call) => call.method === "PUT" && call.path.endsWith("/themes/default"))?.body).toEqual({ theme_id: OTHER_ID });
  });

  it("shows the refusal when an unpublished theme is made the default", async () => {
    mockApi({
      ...themeRoutes({ list: twoThemes() }),
      [`GET ${WS}/themes/${DEFAULT_ID}/where-used`]: () => json(usedBy()),
      [`PUT ${WS}/themes/default`]: () =>
        refusal(409, { code: "theme_not_publishable_default", detail: "the theme has no published version, so it cannot be the default" }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "More actions for Cool" }));
    await user.click(screen.getByRole("menuitem", { name: "Set as default" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(await within(dialog).findByRole("button", { name: "Make default" }));

    expect(await within(dialog).findByText(/cannot be the default/)).toBeInTheDocument();
  });

  it("shows the refusal when an archive is held", async () => {
    mockApi({
      ...themeRoutes({ list: twoThemes() }),
      [`DELETE ${WS}/themes/${OTHER_ID}`]: () => refusal(409, { code: "theme_is_default", detail: "the theme is the workspace default; make another theme the default first" }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "More actions for Cool" }));
    await user.click(screen.getByRole("menuitem", { name: /Archive/ }));
    const dialog = await screen.findByRole("dialog", { name: "Archive Cool?" });
    await user.click(within(dialog).getByRole("button", { name: "Archive" }));

    expect(await within(dialog).findByText(/make another theme the default first/)).toBeInTheDocument();
  });

  it("creates a theme from a name and reports a clash", async () => {
    const calls = mockApi({
      ...themeRoutes({ list: twoThemes() }),
      [`POST ${WS}/themes`]: () => refusal(409, { detail: "a theme with that name already exists" }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "New theme" }));
    const dialog = await screen.findByRole("dialog", { name: "New theme" });
    await user.type(within(dialog).getByLabelText("Name"), "Warm");
    await user.click(within(dialog).getByRole("button", { name: "Create theme" }));

    expect(await within(dialog).findByText("A theme with that name already exists. Choose another.")).toBeInTheDocument();
    expect(calls.find((call) => call.method === "POST")?.body).toEqual({ name: "Warm" });
  });

  it("offers a viewer no new theme and no menu, and an admin the menu", async () => {
    mockApi(themeRoutes({ role: "reader", list: twoThemes() }));
    const viewer = mount();

    await screen.findByRole("link", { name: "Cool" });
    expect(screen.queryByRole("button", { name: "New theme" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /More actions/ })).not.toBeInTheDocument();
    viewer.unmount();

    mockApi(themeRoutes({ list: twoThemes() }));
    const user = userEvent.setup();
    mount();
    await user.click(await screen.findByRole("button", { name: "More actions for Cool" }));
    expect(screen.getByRole("menuitem", { name: "Set as default" })).toBeInTheDocument();
    expect(screen.getByRole("menuitem", { name: /Archive/ })).toBeInTheDocument();
  });

  it("offers a writer a new theme but no default or archive", async () => {
    mockApi(themeRoutes({ role: "editor", list: twoThemes() }));
    mount();

    expect(await screen.findByRole("button", { name: "New theme" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /More actions/ })).not.toBeInTheDocument();
  });
});
