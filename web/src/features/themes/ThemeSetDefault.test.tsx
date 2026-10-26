import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { listItem, templateUse, themeRoutes, tokens, usedBy, WS } from "../../test/themesApi";
import { ThemesPage } from "./ThemesPage";

const FIRST = "77777777-0000-4000-8000-0000000000e1";
const SECOND = "77777777-0000-4000-8000-0000000000e2";
const THIRD = "77777777-0000-4000-8000-0000000000e3";

const themes = (defaultId: string) =>
  [FIRST, SECOND, THIRD].map((id, n) => listItem({ id, name: ["Warm", "Cool", "Plain"][n], current_version: 1, is_default: id === defaultId }, 0, tokens()));

function mount() {
  return renderRoute("/w/ws1/themes", "/w/:ws/themes", <ThemesPage />);
}

describe("the set-default confirm", () => {
  it("shows a loading state, never the no-default text, while the themes are read again", async () => {
    let reads = 0;
    let release: () => void = () => undefined;
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    mockApi({
      ...themeRoutes(),
      [`GET ${WS}/themes`]: async () => {
        reads += 1;
        if (reads > 1) await gate;
        return json(themes(FIRST));
      },
      [`GET ${WS}/themes/${FIRST}/where-used`]: () => json(usedBy({ default_templates: [templateUse(1)] })),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "More actions for Cool" }));
    await user.click(screen.getByRole("menuitem", { name: "Set as default" }));
    const dialog = await screen.findByRole("dialog", { name: "Make Cool the default?" });

    expect(within(dialog).getByText("Checking which templates follow the default")).toBeInTheDocument();
    expect(within(dialog).queryByText("No theme is the default yet.")).not.toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Make default" })).toBeDisabled();
    release();
    expect(await within(dialog).findByText(/1 template follow the default today/)).toBeInTheDocument();
  });

  it("counts the followers of the default as it is now, not as the page loaded it", async () => {
    let reads = 0;
    mockApi({
      ...themeRoutes(),
      [`GET ${WS}/themes`]: () => json(themes(++reads === 1 ? FIRST : SECOND)),
      [`GET ${WS}/themes/${FIRST}/where-used`]: () => json(usedBy({ default_templates: [1, 2, 3, 4, 5].map(templateUse) })),
      [`GET ${WS}/themes/${SECOND}/where-used`]: () => json(usedBy({ default_templates: [1, 2].map(templateUse) })),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "More actions for Plain" }));
    await user.click(screen.getByRole("menuitem", { name: "Set as default" }));

    const dialog = await screen.findByRole("dialog", { name: "Make Plain the default?" });
    expect(await within(dialog).findByText(/2 templates follow the default today/)).toBeInTheDocument();
  });

  it("offers a retry when the read fails, and says there is no default only when there is none", async () => {
    let reads = 0;
    mockApi({
      ...themeRoutes(),
      [`GET ${WS}/themes`]: () => {
        reads += 1;
        if (reads === 2) return new Response("{}", { status: 500, headers: { "content-type": "application/json" } });
        return json(themes(reads === 1 ? FIRST : ""));
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "More actions for Cool" }));
    await user.click(screen.getByRole("menuitem", { name: "Set as default" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(await within(dialog).findByRole("button", { name: "Try again" }));

    expect(await within(dialog).findByText("No theme is the default yet.")).toBeInTheDocument();
  });
});
