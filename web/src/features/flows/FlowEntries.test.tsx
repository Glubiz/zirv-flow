import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it } from "vitest";

import { FLOW_ID, flowRoutes, versionBody, WS } from "../../test/flowsApi";
import { mountEditor as mount } from "../../test/flowsEditor";
import { json, mockApi, problem, type Handler } from "../../test/mockApi";
import { stubReactFlow } from "../../test/reactFlowStubs";

beforeEach(() => {
  stubReactFlow();
});

const FLOW = `${WS}/flows/${FLOW_ID}`;

function series(counts: number[], previous = 0) {
  const days = counts.map((entered, index) => ({ date: `2026-10-${String(index + 1).padStart(2, "0")}`, entered }));
  return {
    from: "2026-10-01",
    to: days.at(-1)?.date ?? "",
    timezone: "UTC",
    days,
    total: counts.reduce((sum, entered) => sum + entered, 0),
    previous_total: previous,
  };
}

function routes(entries: Handler, state = "published"): Record<string, Handler> {
  return {
    ...flowRoutes({ flow: { state, current_version: state === "published" ? 1 : null } }),
    [`GET ${FLOW}/versions`]: () => json(state === "published" ? [versionBody(1)] : []),
    [`GET ${FLOW}/versions/1/counts`]: () => json({ version: 1, nodes: [] }),
    [`GET ${FLOW}/enrollments`]: () => json({ enrollments: [], next_cursor: null }),
    [`GET ${FLOW}/entries`]: entries,
  };
}

async function openResults(user: ReturnType<typeof userEvent.setup>) {
  await user.click(await screen.findByRole("link", { name: "Results" }));
  return screen.findByRole("heading", { name: "Entered per day" });
}

describe("the entries per day on the flow results", () => {
  it("draws the series with a label worked out from the data, the change on the days before, and a table", async () => {
    const calls = mockApi(routes(() => json(series([110, 214, 0, 4488], 3000))));
    const user = userEvent.setup();
    mount();

    const heading = await openResults(user);

    const panel = heading.closest("section") as HTMLElement;
    const chart = await within(panel).findByRole("img", {
      name: "People entering per day over the last 4 days, between 0 and 4,488 a day, 4,812 in total",
    });
    expect(chart).toBeInTheDocument();
    expect(within(panel).getByText("4,812 in 4 days · +60% on the 4 days before")).toBeInTheDocument();
    expect(within(panel).getByText("Days are UTC.")).toBeInTheDocument();
    expect(calls.find((call) => call.path === `${FLOW}/entries`)?.search).toBe("?days=30");

    await user.click(within(panel).getByText("Show as table"));
    expect(within(panel).getByRole("row", { name: "2 Oct 214" })).toBeInTheDocument();
  });

  it("says no one has entered when the total is zero, and draws no axes", async () => {
    mockApi(routes(() => json(series([0, 0, 0]))));
    const user = userEvent.setup();
    mount();

    const panel = (await openResults(user)).closest("section") as HTMLElement;

    expect(await within(panel).findByText("No one has entered this flow in the last 30 days")).toBeInTheDocument();
    expect(within(panel).queryByRole("img")).not.toBeInTheDocument();
  });

  it("shows a loading state in the panel only", async () => {
    mockApi(routes(() => new Promise<Response>(() => undefined)));
    const user = userEvent.setup();
    mount();

    const panel = (await openResults(user)).closest("section") as HTMLElement;

    expect(within(panel).getByRole("status")).toHaveTextContent("Loading entries");
    expect(await screen.findByRole("region", { name: "Enrollments" })).toBeInTheDocument();
  });

  it("shows an error with a retry that reads the series again, leaving the rest of the results", async () => {
    let asked = 0;
    mockApi(
      routes(() => {
        asked += 1;
        return asked === 1 ? problem(500, "Entries failed", "The read failed.") : json(series([1, 2]));
      }),
    );
    const user = userEvent.setup();
    mount();

    const panel = (await openResults(user)).closest("section") as HTMLElement;

    expect(await within(panel).findByText("Entries failed")).toBeInTheDocument();
    expect(screen.getByRole("region", { name: "Enrollments" })).toBeInTheDocument();
    await user.click(within(panel).getByRole("button", { name: "Try again" }));
    expect(await within(panel).findByRole("img")).toBeInTheDocument();
  });
});
