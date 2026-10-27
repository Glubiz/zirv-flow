import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { themeDetail, themeRoutes, THEME_ID, WS } from "../../test/themesApi";
import { ThemePage } from "./ThemePage";

describe("archiving from the editor", () => {
  it("leaves for the list with no unsaved-changes prompt, even with an edit still in the debounce", async () => {
    mockApi({
      ...themeRoutes(),
      [`DELETE ${WS}/themes/${THEME_ID}`]: () => json(themeDetail({ archived_at: "2026-10-12T09:00:00Z" })),
    });
    const user = userEvent.setup();
    const { router } = renderRoute(`/w/ws1/themes/${THEME_ID}`, "/w/:ws/themes/:id", <ThemePage />);
    await screen.findByTitle("Preview of the theme");

    fireEvent.change(screen.getByLabelText("Primary"), { target: { value: "#112233" } });
    expect(screen.getByText("Unsaved changes")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "More actions for Warm" }));
    await user.click(screen.getByRole("menuitem", { name: /Archive/ }));
    const dialog = await screen.findByRole("dialog", { name: "Archive Warm?" });
    await user.click(within(dialog).getByRole("button", { name: "Archive" }));

    await waitFor(() => expect(router.state.location.pathname).toBe("/w/ws1/themes"));
    expect(screen.queryByText("Leave without saving?")).not.toBeInTheDocument();
  });
});
