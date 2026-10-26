import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { assetBody, FakeRequest, listBody } from "../../test/assetsApi";
import { json, mockApi } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { saves, themeDetail, themeRoutes, THEME_ID, tokens, WS } from "../../test/themesApi";
import { ThemePage } from "./ThemePage";

const LOGO = assetBody(1, { name: "logo.png" });

beforeEach(() => {
  vi.stubGlobal("XMLHttpRequest", FakeRequest);
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function mount() {
  return renderRoute(`/w/ws1/themes/${THEME_ID}`, "/w/:ws/themes/:id", <ThemePage />);
}

describe("the logo setting", () => {
  it("offers library images only, stores the picked asset id, and Remove clears it", async () => {
    const calls = mockApi({
      ...themeRoutes({ detail: themeDetail({}, tokens({ logo: null })) }),
      [`GET ${WS}/assets`]: () => json(listBody([LOGO])),
      [`GET ${WS}/assets/${LOGO.id}`]: () => json({ ...LOGO, used_by: [] }),
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Change" }));
    const dialog = await screen.findByRole("dialog");
    await user.click(await within(dialog).findByRole("radio", { name: /logo\.png/ }));
    expect(within(dialog).queryByRole("button", { name: "Use a web address" })).not.toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "Use image" }));

    await waitFor(() => expect(saves(calls)).toHaveLength(1), { timeout: 4000 });
    expect(saves(calls)[0]?.tokens["logo"]).toBe(LOGO.id);

    fireEvent.click(await screen.findByRole("button", { name: "Remove" }));
    await waitFor(() => expect(saves(calls)).toHaveLength(2), { timeout: 4000 });
    expect(saves(calls)[1]?.tokens["logo"]).toBeNull();
  });
});
