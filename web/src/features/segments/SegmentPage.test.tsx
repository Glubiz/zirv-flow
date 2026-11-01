import { act, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Route, Routes } from "react-router";
import { describe, expect, it } from "vitest";

import { json, mockApi, problem, type Call, type Handler } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import {
  actor,
  catalogRoutes,
  detail,
  LIVE_AGE,
  PERSON_ID,
  previewBody,
  refusal,
  SEGMENT_ID,
  VERSION_1,
  WS,
} from "../../test/segmentsApi";
import { NewSegmentPage } from "./NewSegmentPage";
import { REFRESH_RELOAD_MS, SegmentPage } from "./SegmentPage";

const SEGMENT = `${WS}/segments/${SEGMENT_ID}`;

function base(kind: "writer" | "reader" = "writer"): Record<string, Handler> {
  return {
    ...actor(kind),
    ...catalogRoutes(),
    [`POST ${WS}/segments/preview`]: () => json(previewBody()),
    [`GET ${SEGMENT}`]: () => json(detail()),
    [`GET ${SEGMENT}/where-used`]: () => json({ flows: [], segments: [] }),
    [`GET ${SEGMENT}/size`]: () => json(sizeSeries([0, 0])),
    [`GET ${SEGMENT}/history`]: () => json({ items: [], data_age: LIVE_AGE }),
  };
}

function sizeSeries(sizes: number[]) {
  const days = sizes.map((size, index) => ({ date: `2026-10-${String(index + 1).padStart(2, "0")}`, size }));
  return { from: "2026-10-01", to: days.at(-1)?.date ?? "", timezone: "UTC", days, current: sizes.at(-1) ?? 0 };
}

function mountPage(search = "?tab=rules") {
  return renderRoute(
    `/w/ws1/segments/${SEGMENT_ID}${search}`,
    "/w/:ws/*",
    <Routes>
      <Route path="segments/:id" element={<SegmentPage />} />
      <Route path="segments" element={<p>The segment list was reached</p>} />
    </Routes>,
  );
}

function bodyOf(calls: Call[], method: string, path: string): unknown {
  return calls.filter((call) => call.method === method && call.path === path).at(-1)?.body;
}

async function openTab(user: ReturnType<typeof userEvent.setup>, name: string) {
  await user.click(await screen.findByRole("link", { name }));
}

describe("saving a segment", () => {
  it("creates a segment and goes to its page", async () => {
    const calls = mockApi({
      ...base(),
      [`POST ${WS}/segments`]: () => json(detail(), 201),
    });
    const user = userEvent.setup();
    renderRoute(
      "/w/ws1/segments/new",
      "/w/:ws/*",
      <Routes>
        <Route path="segments/new" element={<NewSegmentPage />} />
        <Route path="segments/:id" element={<p>The new segment page was reached</p>} />
      </Routes>,
    );

    await user.type(await screen.findByLabelText("Name"), "  Big spenders ");
    await user.type(screen.getByLabelText("Description"), "People who spend");
    await user.selectOptions(screen.getByLabelText("Re-evaluate"), "1440");
    await user.click(screen.getByRole("button", { name: "Add condition to All of" }));
    const condition = screen.getByRole("group", { name: "Condition 1" });
    await user.selectOptions(within(condition).getByLabelText("Field"), "city");
    await user.type(within(condition).getByLabelText("Value"), "Aarhus");
    await user.click(screen.getByRole("button", { name: "Create segment" }));

    expect(await screen.findByText("The new segment page was reached")).toBeInTheDocument();
    expect(bodyOf(calls, "POST", `${WS}/segments`)).toEqual({
      name: "Big spenders",
      description: "People who spend",
      cadence_minutes: 1440,
      definition: { v: 1, all: [{ trait: "city", op: "eq", value: "Aarhus" }] },
    });
  });

  it("offers the thirteen cadences in words", async () => {
    mockApi(base());
    mountPage();

    const options = within(await screen.findByLabelText("Re-evaluate")).getAllByRole("option");
    expect(options).toHaveLength(13);
    expect(options[0]).toHaveTextContent("every 5 minutes");
    expect(options[12]).toHaveTextContent("every week");
  });

  it("saves a changed definition as a new version and shows its number", async () => {
    const calls = mockApi({
      ...base(),
      [`PATCH ${SEGMENT}`]: () =>
        json(
          detail(
            {},
            { version: 2, definition: { v: 1, all: [{ trait: "city", op: "eq", value: "Odense" }] } },
          ),
        ),
    });
    const user = userEvent.setup();
    mountPage();

    const value = await screen.findByLabelText("Value");
    await user.clear(value);
    await user.type(value, "Odense");
    expect(await screen.findByText("Unsaved changes")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(await screen.findByText("Saved as version 2.")).toBeInTheDocument();
    expect(bodyOf(calls, "PATCH", SEGMENT)).toEqual({ definition: { v: 1, all: [{ trait: "city", op: "eq", value: "Odense" }] } });
    await waitFor(() => expect(screen.queryByText("Unsaved changes")).not.toBeInTheDocument());
  });

  it("is clean after a save even though the API returns the keys in another order", async () => {
    const odense = { v: 1, all: [{ trait: "city", op: "eq", value: "Odense" }] };
    mockApi({ ...base(), [`PATCH ${SEGMENT}`]: () => json(detail({}, { version: 2, definition: odense })) });
    const user = userEvent.setup();
    mountPage();

    const value = await screen.findByLabelText("Value");
    await user.clear(value);
    await user.type(value, "Odense");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(await screen.findByText("Saved as version 2.")).toBeInTheDocument();
    expect(screen.queryByText("Unsaved changes")).not.toBeInTheDocument();
    expect(screen.getByText("Version 2 is saved.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
    await user.click(screen.getByRole("link", { name: "Segments" }));
    expect(await screen.findByText("The segment list was reached")).toBeInTheDocument();
  });

  it("is clean after a rename with spaces around the name", async () => {
    const calls = mockApi({ ...base(), [`PATCH ${SEGMENT}`]: () => json(detail({ name: "Renamed" })) });
    const user = userEvent.setup();
    mountPage();

    const name = await screen.findByLabelText("Name");
    await user.clear(name);
    await user.type(name, "  Renamed  ");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(await screen.findByText("Saved.")).toBeInTheDocument();
    expect(bodyOf(calls, "PATCH", SEGMENT)).toEqual({ name: "Renamed" });
    expect(screen.queryByText("Unsaved changes")).not.toBeInTheDocument();
    expect(screen.getByLabelText("Name")).toHaveValue("Renamed");
    expect(screen.getByRole("button", { name: "Save" })).toBeDisabled();
  });

  it("updates only the header when the definition is untouched", async () => {
    const calls = mockApi({
      ...base(),
      [`PATCH ${SEGMENT}`]: () => json(detail({ name: "Renamed", cadence_minutes: 30 })),
    });
    const user = userEvent.setup();
    mountPage();

    const name = await screen.findByLabelText("Name");
    await user.clear(name);
    await user.type(name, "Renamed");
    await user.selectOptions(screen.getByLabelText("Re-evaluate"), "30");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(await screen.findByText("Saved.")).toBeInTheDocument();
    expect(bodyOf(calls, "PATCH", SEGMENT)).toEqual({ name: "Renamed", cadence_minutes: 30 });
  });

  it("asks in its own dialog before leaving with unsaved changes", async () => {
    mockApi(base());
    const user = userEvent.setup();
    mountPage();

    const name = await screen.findByLabelText("Name");
    await user.type(name, "!");
    await user.click(screen.getByRole("link", { name: "Segments" }));

    const dialog = await screen.findByRole("dialog", { name: "Leave without saving?" });
    await user.click(within(dialog).getByRole("button", { name: "Keep editing" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(screen.queryByText("The segment list was reached")).not.toBeInTheDocument();

    await user.click(screen.getByRole("link", { name: "Segments" }));
    await user.click(within(await screen.findByRole("dialog")).getByRole("button", { name: "Leave without saving" }));
    expect(await screen.findByText("The segment list was reached")).toBeInTheDocument();
  });

  it("leaves at once when nothing is unsaved", async () => {
    mockApi(base());
    const user = userEvent.setup();
    mountPage();

    await screen.findByLabelText("Name");
    await user.click(screen.getByRole("link", { name: "Segments" }));

    expect(await screen.findByText("The segment list was reached")).toBeInTheDocument();
  });

  it("shows a refusal of the save on the conditions", async () => {
    mockApi({
      ...base(),
      [`PATCH ${SEGMENT}`]: () => refusal(422, "segment_invalid", [{ pointer: "/all/0/value", code: "value_too_long" }]),
    });
    const user = userEvent.setup();
    mountPage();

    await user.type(await screen.findByLabelText("Value"), "x");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(await screen.findByText("The definition has problems")).toBeInTheDocument();
    expect(screen.getByLabelText("Value")).toHaveAttribute("aria-invalid", "true");
  });
});

describe("segment page", () => {
  it("shows the header facts and the four tabs, with Overview first", async () => {
    mockApi(base());
    mountPage("");

    expect(await screen.findByRole("heading", { name: "Big spenders" })).toBeInTheDocument();
    expect(screen.getByText("re-evaluated every hour")).toBeInTheDocument();
    expect(screen.getByText("7")).toBeInTheDocument();
    const tabs = within(screen.getByRole("navigation", { name: "Segment sections" })).getAllByRole("link");
    expect(tabs.map((tab) => tab.textContent)).toEqual(["Overview", "Rules", "People", "History"]);
    expect(screen.getByRole("link", { name: "Overview" })).toHaveAttribute("aria-current", "page");
    expect(screen.getByRole("navigation", { name: "Breadcrumb" })).toHaveTextContent("Audience/Segments");
    expect(screen.queryByText("All segments")).not.toBeInTheDocument();
  });

  it("describes who is in the segment and lists what it is built from", async () => {
    mockApi({
      ...base(),
      [`GET ${SEGMENT}`]: () => json(detail({}, { deps: { traits: ["city"], lists: [], segments: [], consent: [] } })),
    });
    mountPage("");

    const panel = await screen.findByRole("region", { name: "Who is in it" });
    expect(within(panel).getByText(/^People who match all of these: /)).toBeInTheDocument();
    const related = screen.getByRole("complementary", { name: "Related" });
    expect(within(related).getByText("Built from")).toBeInTheDocument();
    expect(within(related).getByText("city")).toBeInTheDocument();
  });

  it("draws the size per day with a label worked out from the data, the count now, and a table", async () => {
    const calls = mockApi({ ...base(), [`GET ${SEGMENT}/size`]: () => json(sizeSeries([1200, 1180, 1204])) });
    const user = userEvent.setup();
    mountPage("");

    const panel = (await screen.findByRole("heading", { name: "Size, last 30 days" })).closest("section") as HTMLElement;
    expect(
      await within(panel).findByRole("img", {
        name: "Segment size at the end of each of the last 3 days, between 1,180 and 1,204 people, 1,204 now",
      }),
    ).toBeInTheDocument();
    expect(within(panel).getByText("1,204 people now")).toBeInTheDocument();
    expect(within(panel).getByText("Days are UTC.")).toBeInTheDocument();
    expect(calls.find((call) => call.path === `${SEGMENT}/size`)?.search).toBe("?days=30");
    await user.click(within(panel).getByText("Show as table"));
    expect(within(panel).getByRole("row", { name: "2 Oct 1,180" })).toBeInTheDocument();
  });

  it("says no one has been in the segment when every day is zero, and draws no chart", async () => {
    mockApi(base());
    mountPage("");

    const panel = (await screen.findByRole("heading", { name: "Size, last 30 days" })).closest("section") as HTMLElement;
    expect(await within(panel).findByText("No one has been in this segment in the last 30 days")).toBeInTheDocument();
    expect(within(panel).queryByRole("img")).not.toBeInTheDocument();
  });

  it("shows the size loading in its own panel, then an error with a retry that reads it again", async () => {
    let asked = 0;
    mockApi({
      ...base(),
      [`GET ${SEGMENT}/size`]: () => {
        asked += 1;
        if (asked === 1) return new Promise<Response>((resolve) => setTimeout(() => resolve(problem(500, "Size failed", "The read failed.")), 20));
        return json(sizeSeries([3, 4]));
      },
    });
    const user = userEvent.setup();
    mountPage("");

    const panel = (await screen.findByRole("heading", { name: "Size, last 30 days" })).closest("section") as HTMLElement;
    expect(within(panel).getByRole("status")).toHaveTextContent("Loading size");
    expect(screen.getByRole("region", { name: "Who is in it" })).toBeInTheDocument();
    expect(await within(panel).findByText("Size failed")).toBeInTheDocument();
    await user.click(within(panel).getByRole("button", { name: "Try again" }));
    expect(await within(panel).findByRole("img")).toBeInTheDocument();
  });

  it("lists the flows and segments that use it, with Live or Draft beside a flow", async () => {
    mockApi({
      ...base(),
      [`GET ${SEGMENT}/where-used`]: () =>
        json({
          flows: [
            { flow_id: "f1", name: "Welcome", state: "published", stage: "live", version: 2, uses: ["trigger"] },
            { flow_id: "f2", name: "Winback", state: "draft", stage: "draft", version: null, uses: ["condition"] },
          ],
          segments: [{ segment_id: "s9", name: "Big and active" }],
        }),
    });
    mountPage("");

    const related = await screen.findByRole("complementary", { name: "Related" });
    expect(within(related).getByText("Used by flows")).toBeInTheDocument();
    expect(within(related).getByRole("link", { name: "Welcome" })).toHaveAttribute("href", "/w/ws1/flows/f1");
    expect(within(related).getByText("Live")).toBeInTheDocument();
    expect(within(related).getByText("Draft")).toBeInTheDocument();
    expect(within(related).getByText("Used by segments")).toBeInTheDocument();
    expect(within(related).getByRole("link", { name: "Big and active" })).toHaveAttribute("href", "/w/ws1/segments/s9");
  });

  it("leaves out a Used by group that is empty", async () => {
    mockApi({
      ...base(),
      [`GET ${SEGMENT}/where-used`]: () => json({ flows: [], segments: [{ segment_id: "s9", name: "Big and active" }] }),
    });
    mountPage("");

    const related = await screen.findByRole("complementary", { name: "Related" });
    expect(within(related).queryByText("Used by flows")).not.toBeInTheDocument();
    expect(within(related).getByText("Used by segments")).toBeInTheDocument();
  });

  it("draws no Related panel when the version reads nothing", async () => {
    mockApi(base());
    mountPage("");

    await screen.findByRole("region", { name: "Who is in it" });
    expect(screen.queryByRole("complementary", { name: "Related" })).not.toBeInTheDocument();
  });

  it("keeps the tab in the URL and opens Overview for an unknown tab", async () => {
    mockApi({ ...base(), [`GET ${SEGMENT}/members`]: () => json({ items: [], data_age: LIVE_AGE }) });
    const user = userEvent.setup();
    const view = mountPage("?tab=nonsense");

    expect(await screen.findByRole("link", { name: "Overview" })).toHaveAttribute("aria-current", "page");
    await user.click(screen.getByRole("link", { name: "People" }));
    expect(view.router.state.location.search).toBe("?tab=people");
    await act(() => view.router.navigate(-1));
    await waitFor(() => expect(screen.getByRole("link", { name: "Overview" })).toHaveAttribute("aria-current", "page"));
  });

  it("hides Edit rules from someone who cannot write", async () => {
    mockApi(base("reader"));
    mountPage("");

    await screen.findByRole("region", { name: "Who is in it" });
    expect(screen.queryByRole("button", { name: "Edit rules" })).not.toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "Edit rules" })).not.toBeInTheDocument();
  });

  it("shows a loading state and then an error with a retry", async () => {
    let asked = 0;
    mockApi({
      ...base(),
      [`GET ${SEGMENT}`]: () => {
        asked += 1;
        return asked === 1 ? problem(404, "Not Found", "no such segment") : json(detail());
      },
    });
    const user = userEvent.setup();
    mountPage();

    expect(await screen.findByText("Not Found")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("heading", { name: "Big spenders" })).toBeInTheDocument();
  });

  it("shows a loading state while the segment loads", async () => {
    mockApi({ ...base(), [`GET ${SEGMENT}`]: () => new Promise<Response>(() => undefined) });
    mountPage();

    expect(await screen.findByText("Loading the segment")).toBeInTheDocument();
  });
});

function memberItem(n: number) {
  return {
    id: `55555555-0000-4000-8000-0000000000${String(n).padStart(2, "0")}`,
    display: { kind: "email", masked: false, value: `person${n}@example.test` },
    entered_at: "2026-10-05T09:00:00Z",
  };
}

describe("members tab", () => {
  it("shows the first page and loads the next on request", async () => {
    const calls = mockApi({
      ...base(),
      [`GET ${SEGMENT}/members`]: (request) =>
        new URL(request.url).searchParams.get("cursor") === "c1"
          ? json({ items: [memberItem(2)], computed_at: "2026-10-10T09:00:00Z", data_age: LIVE_AGE, next_cursor: null })
          : json({ items: [memberItem(1)], computed_at: "2026-10-10T09:00:00Z", data_age: LIVE_AGE, next_cursor: "c1" }),
    });
    const user = userEvent.setup();
    mountPage();
    await openTab(user, "People");

    const table = await screen.findByRole("table", { name: "People in this segment" });
    expect(within(table).getByRole("link", { name: "person1@example.test" })).toHaveAttribute(
      "href",
      `/w/ws1/people/${memberItem(1).id}`,
    );
    await user.click(screen.getByRole("button", { name: "Load more" }));

    expect(await within(table).findByRole("link", { name: "person2@example.test" })).toBeInTheDocument();
    expect(within(table).getByRole("link", { name: "person1@example.test" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Load more" })).not.toBeInTheDocument();
    expect(calls.filter((call) => call.path === `${SEGMENT}/members`).map((call) => call.search)).toEqual([
      "?limit=50",
      "?limit=50&cursor=c1",
    ]);
  });

  it("shows loading and empty states", async () => {
    mockApi({ ...base(), [`GET ${SEGMENT}/members`]: () => new Promise<Response>(() => undefined) });
    const user = userEvent.setup();
    const first = mountPage();
    await openTab(user, "People");
    expect(await screen.findByText("Loading members")).toBeInTheDocument();
    first.unmount();

    mockApi({ ...base(), [`GET ${SEGMENT}/members`]: () => json({ items: [], data_age: LIVE_AGE }) });
    mountPage();
    await openTab(user, "People");
    expect(await screen.findByText("No one is in this segment")).toBeInTheDocument();
  });

  it("shows an error with a retry", async () => {
    let asked = 0;
    mockApi({
      ...base(),
      [`GET ${SEGMENT}/members`]: () => {
        asked += 1;
        return asked === 1 ? problem(500, "Internal Server Error", "internal error") : json({ items: [memberItem(1)], data_age: LIVE_AGE });
      },
    });
    const user = userEvent.setup();
    mountPage();
    await openTab(user, "People");

    await user.click(await screen.findByRole("button", { name: "Try again" }));
    expect(await screen.findByRole("link", { name: "person1@example.test" })).toBeInTheDocument();
  });
});

describe("history tab", () => {
  const entry = (kind: string, cause: string, version: number) => ({
    person_id: PERSON_ID,
    kind,
    cause,
    version,
    at: "2026-10-06T09:00:00Z",
    display: { kind: "email", masked: false, value: "ada@example.test" },
  });

  it("lists joins and leaves with the cause in words, and pages", async () => {
    mockApi({
      ...base(),
      [`GET ${SEGMENT}/history`]: (request) =>
        new URL(request.url).searchParams.get("cursor") === "h1"
          ? json({ items: [entry("entered", "initial", 1)], next_cursor: null })
          : json({ items: [entry("left", "data_change", 2), entry("entered", "manual", 2)], next_cursor: "h1" }),
    });
    const user = userEvent.setup();
    mountPage();
    await openTab(user, "History");

    const table = await screen.findByRole("table", { name: "When people joined and left, newest first" });
    const rows = within(table).getAllByRole("row");
    expect(within(rows[1]!).getByText("Left")).toBeInTheDocument();
    expect(within(rows[1]!).getByText("data change")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("Joined")).toBeInTheDocument();
    expect(within(rows[2]!).getByText("manual refresh")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Load more" }));
    expect(await within(table).findByText("first computation")).toBeInTheDocument();
  });

  it("shows loading, empty and error states", async () => {
    mockApi({ ...base(), [`GET ${SEGMENT}/history`]: () => new Promise<Response>(() => undefined) });
    const user = userEvent.setup();
    const first = mountPage();
    await openTab(user, "History");
    expect(await screen.findByText("Loading history")).toBeInTheDocument();
    first.unmount();

    mockApi({ ...base(), [`GET ${SEGMENT}/history`]: () => json({ items: [] }) });
    const second = mountPage();
    await openTab(user, "History");
    expect(await screen.findByText("No history yet")).toBeInTheDocument();
    second.unmount();

    mockApi({ ...base(), [`GET ${SEGMENT}/history`]: () => problem(500, "Internal Server Error", "internal error") });
    mountPage();
    await openTab(user, "History");
    expect(await screen.findByText("Internal Server Error")).toBeInTheDocument();
  });
});

describe("versions tab", () => {
  const VERSION_2 = {
    ...VERSION_1,
    version: 2,
    created_by: "66666666-0000-4000-8000-000000000001",
    created_at: "2026-10-08T09:00:00Z",
    definition: { v: 1, all: [{ trait: "city", op: "eq", value: "Odense" }] },
  };

  it("shows an older version in the builder, read-only, and says which is current", async () => {
    mockApi({
      ...base(),
      [`GET ${SEGMENT}`]: () => json(detail({}, VERSION_2)),
      [`GET ${SEGMENT}/versions`]: () => json([VERSION_2, VERSION_1]),
    });
    const user = userEvent.setup();
    mountPage();
    await openTab(user, "History");

    const table = await screen.findByRole("table", { name: "Every saved version, newest first" });
    expect(within(table).getAllByRole("row")).toHaveLength(3);
    await user.click(screen.getByRole("button", { name: "Show version 1" }));

    expect(await screen.findByText(/This is version 1; the current version is 2\./)).toBeInTheDocument();
    const panel = screen.getByRole("region", { name: "Version 1" });
    const value = await within(panel).findByLabelText("Value");
    expect(value).toHaveValue("Aarhus");
    expect(value).toBeDisabled();
    expect(within(panel).queryByRole("button", { name: /^Add / })).not.toBeInTheDocument();
  });

  it("shows loading, empty and error states", async () => {
    mockApi({ ...base(), [`GET ${SEGMENT}/versions`]: () => new Promise<Response>(() => undefined) });
    const user = userEvent.setup();
    const first = mountPage();
    await openTab(user, "History");
    expect(await screen.findByText("Loading versions")).toBeInTheDocument();
    first.unmount();

    mockApi({ ...base(), [`GET ${SEGMENT}/versions`]: () => json([]) });
    const second = mountPage();
    await openTab(user, "History");
    expect(await screen.findByText("No versions")).toBeInTheDocument();
    second.unmount();

    mockApi({ ...base(), [`GET ${SEGMENT}/versions`]: () => problem(500, "Internal Server Error", "internal error") });
    mountPage();
    await openTab(user, "History");
    expect(await screen.findByText("Internal Server Error")).toBeInTheDocument();
  });
});

describe("refresh now", () => {
  it("queues a refresh, disables the button, and reads the segment again", async () => {
    let computed = "2026-10-10T09:00:00Z";
    const calls = mockApi({
      ...base(),
      [`GET ${SEGMENT}`]: () => json(detail({ computed_at: computed })),
      [`POST ${SEGMENT}/refresh`]: () => {
        computed = "2026-10-17T09:00:00Z";
        return json({ queued: true }, 202);
      },
    });
    const user = userEvent.setup();
    mountPage();

    await user.click(await screen.findByRole("button", { name: "Refresh now" }));

    expect(await screen.findByText("Refresh queued")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Refresh now" })).toBeDisabled();
    await waitFor(() => expect(screen.getByRole("button", { name: "Refresh now" })).toBeEnabled(), { timeout: REFRESH_RELOAD_MS + 3000 });
    expect(calls.filter((call) => call.method === "GET" && call.path === SEGMENT)).toHaveLength(2);
    expect(screen.queryByText("Refresh queued")).not.toBeInTheDocument();
  });

  it("shows a failed refresh inline and lets it be tried again", async () => {
    mockApi({
      ...base(),
      [`POST ${SEGMENT}/refresh`]: () => problem(500, "Internal Server Error", "internal error"),
    });
    const user = userEvent.setup();
    mountPage();

    await user.click(await screen.findByRole("button", { name: "Refresh now" }));

    expect(await screen.findByText("Internal Server Error")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Refresh now" })).toBeEnabled();
  });

  it("is not offered to someone who cannot write", async () => {
    mockApi(base("reader"));
    mountPage();

    await screen.findByRole("heading", { name: "Big spenders" });
    expect(screen.queryByRole("button", { name: "Refresh now" })).not.toBeInTheDocument();
  });
});
