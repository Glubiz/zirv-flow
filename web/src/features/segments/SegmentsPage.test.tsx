import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import { actor, SEGMENT_ID, segmentRow, WS } from "../../test/segmentsApi";
import { SegmentsPage } from "./SegmentsPage";

function mount() {
  return renderRoute("/w/ws1/segments", "/w/:ws/segments", <SegmentsPage />);
}

describe("segments list", () => {
  it("shows a loading state while the segments load", async () => {
    mockApi({ ...actor("writer"), [`GET ${WS}/segments`]: () => new Promise<Response>(() => undefined) });
    mount();

    expect(await screen.findByText("Loading segments")).toBeInTheDocument();
  });

  it("offers a first segment when there are none and the person can write", async () => {
    mockApi({ ...actor("writer"), [`GET ${WS}/segments`]: () => json([]) });
    mount();

    expect(await screen.findByText("No segments yet")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Create the first segment" })).toBeInTheDocument();
  });

  it("shows a retry when loading fails, and loads again", async () => {
    let attempts = 0;
    mockApi({
      ...actor("writer"),
      [`GET ${WS}/segments`]: () => {
        attempts += 1;
        return attempts === 1 ? problem(500, "Internal Server Error", "internal error") : json([segmentRow()]);
      },
    });
    const user = userEvent.setup();
    mount();

    await user.click(await screen.findByRole("button", { name: "Try again" }));

    expect(await screen.findByText("Big spenders")).toBeInTheDocument();
    expect(attempts).toBe(2);
  });

  it("lists each segment with its cadence in words and says when one has not been computed", async () => {
    mockApi({
      ...actor("writer"),
      [`GET ${WS}/segments`]: () =>
        json([
          segmentRow(),
          segmentRow({ id: "x2", name: "Fresh", computed_at: null, cadence_minutes: 10080, member_count: 0 }),
        ]),
    });
    mount();

    const table = await screen.findByRole("table", { name: "Segments" });
    const rows = within(table).getAllByRole("row");
    expect(within(rows[1]!).getByRole("link", { name: "Big spenders" })).toHaveAttribute("href", `/w/ws1/segments/${SEGMENT_ID}`);
    expect(within(rows[1]!).getByText("re-evaluated every hour")).toBeInTheDocument();
    expect(within(rows[1]!).getByText(/^computed /)).toBeInTheDocument();
    expect(within(rows[2]!).getByText("not computed yet")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("re-evaluated every week")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "New segment" })).toBeInTheDocument();
  });

  it("filters by the search term in the URL and says when nothing matches", async () => {
    mockApi({
      ...actor("writer"),
      [`GET ${WS}/segments`]: () => json([segmentRow(), segmentRow({ id: "x2", name: "Lapsed" })]),
    });
    const user = userEvent.setup();
    renderRoute("/w/ws1/segments?q=laps", "/w/:ws/segments", <SegmentsPage />);

    const table = await screen.findByRole("table", { name: "Segments" });
    expect(within(table).getAllByRole("row")).toHaveLength(2);
    expect(screen.getByRole("link", { name: "All 2" })).toHaveAttribute("aria-current", "page");
    await user.clear(screen.getByRole("searchbox", { name: "Search segments" }));
    await user.type(screen.getByRole("searchbox", { name: "Search segments" }), "zzz");
    expect(await screen.findByText("No segments match")).toBeInTheDocument();
  });

  it("is read-only for someone who cannot write", async () => {
    mockApi({ ...actor("reader"), [`GET ${WS}/segments`]: () => json([segmentRow()]) });
    mount();

    expect(await screen.findByText("Big spenders")).toBeInTheDocument();
    expect(await screen.findByText("You can look at segments but not change them.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "New segment" })).not.toBeInTheDocument();
  });
});
