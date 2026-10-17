import { act, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useNavigate } from "react-router";
import { afterEach, describe, expect, it, vi } from "vitest";

import { json, mockApi } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { actor, catalogRoutes, detail, previewBody, SEGMENT_ID, WS } from "../../test/segmentsApi";
import { SegmentPage } from "./SegmentPage";

const PAGE = `/w/ws1/segments/${SEGMENT_ID}`;

function Switcher() {
  const navigate = useNavigate();
  return (
    <button type="button" onClick={() => void navigate("/w/ws2/segments")}>
      Switch workspace
    </button>
  );
}

function mount() {
  mockApi({
    ...actor("writer"),
    ...catalogRoutes(),
    [`POST ${WS}/segments/preview`]: () => json(previewBody()),
    [`GET ${WS}/segments/${SEGMENT_ID}`]: () => json(detail()),
  });
  return renderRoute(
    PAGE,
    "/w/:ws/segments/:id",
    <>
      <SegmentPage />
      <Switcher />
    </>,
    ["/w/ws1/segments", PAGE],
  );
}

async function makeDirty(user: ReturnType<typeof userEvent.setup>) {
  await user.type(await screen.findByLabelText("Name"), "!");
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("leave guard", () => {
  it("holds a programmatic navigation, and lets it go on confirmation", async () => {
    const view = mount();
    const user = userEvent.setup();
    await makeDirty(user);

    await user.click(screen.getByRole("button", { name: "Switch workspace" }));
    const dialog = await screen.findByRole("dialog", { name: "Leave without saving?" });
    await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
    expect(view.router.state.location.pathname).toBe(PAGE);

    await user.click(screen.getByRole("button", { name: "Switch workspace" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Leave without saving" }));
    expect(view.router.state.location.pathname).toBe("/w/ws2/segments");
  });

  it("holds Back, and lets it go on confirmation", async () => {
    const view = mount();
    const user = userEvent.setup();
    await makeDirty(user);

    await act(() => view.router.navigate(-1));
    const dialog = await screen.findByRole("dialog", { name: "Leave without saving?" });
    await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
    expect(view.router.state.location.pathname).toBe(PAGE);

    await act(() => view.router.navigate(-1));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Leave without saving" }));
    expect(view.router.state.location.pathname).toBe("/w/ws1/segments");
  });

  it("lets Back through at once when nothing is unsaved", async () => {
    const view = mount();
    await screen.findByLabelText("Name");

    await act(() => view.router.navigate(-1));

    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(view.router.state.location.pathname).toBe("/w/ws1/segments");
  });

  it("warns on closing the tab only while there are unsaved changes", async () => {
    const add = vi.spyOn(window, "addEventListener");
    const remove = vi.spyOn(window, "removeEventListener");
    const unloads = (spy: typeof add) => spy.mock.calls.filter(([type]) => type === "beforeunload").length;
    mount();
    const user = userEvent.setup();

    const name = await screen.findByLabelText("Name");
    expect(unloads(add)).toBe(0);

    await user.type(name, "!");
    expect(unloads(add)).toBe(1);

    await user.type(name, "{Backspace}");
    expect(unloads(remove)).toBe(1);
    expect(screen.queryByText("Unsaved changes")).not.toBeInTheDocument();
  });
});
