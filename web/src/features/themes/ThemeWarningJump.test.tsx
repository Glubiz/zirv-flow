import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, describe, expect, it, vi } from "vitest";

import { mockApi } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { contrastIssue, previewBody, themeRoutes, THEME_ID } from "../../test/themesApi";
import { ThemePage } from "./ThemePage";

beforeAll(() => {
  Element.prototype.scrollIntoView = vi.fn();
});

function mount() {
  return renderRoute(`/w/ws1/themes/${THEME_ID}`, "/w/:ws/themes/:id", <ThemePage />);
}

const blocking = { code: "invalid_setting", severity: "blocking", at: "/theme/content_width" };
const warning = { code: "invalid_url", severity: "warning", at: "/theme/web_font_url", message: "an https URL without spaces" };

describe("the warnings pill", () => {
  it("goes back to the editor from another tab and focuses the first warning, not a blocking issue before it", async () => {
    mockApi(themeRoutes({ preview: () => previewBody({ rendered: false, html: null, issues: [blocking, warning] }) }));
    const user = userEvent.setup();
    mount();

    const pill = await screen.findByRole("button", { name: "1 warning" }, { timeout: 4000 });
    await user.click(screen.getByRole("tab", { name: "Versions" }));
    await user.click(pill);

    expect(screen.getByRole("tab", { name: "Editor" })).toHaveAttribute("aria-selected", "true");
    const items = await screen.findAllByRole("listitem");
    const item = items.find((entry) => entry.textContent.includes("Warning"));
    await waitFor(() => expect(item).toHaveFocus());
    expect(items.find((entry) => entry.textContent.includes("Blocks publishing"))).not.toHaveFocus();
  });

  it("focuses a contrast warning under its pair when there is one", async () => {
    mockApi(themeRoutes({ preview: () => previewBody({ issues: [blocking, contrastIssue("/theme/button_text")] }) }));
    const user = userEvent.setup();
    mount();

    const pill = await screen.findByRole("button", { name: "1 warning" }, { timeout: 4000 });
    await user.click(screen.getByRole("tab", { name: "Where used" }));
    await user.click(pill);

    const note = await screen.findByText("Button text on Button is 4.2:1. Aim for 4.5:1.");
    await waitFor(() => expect(note).toHaveFocus());
  });
});
