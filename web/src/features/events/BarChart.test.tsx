import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { BarChart, niceMax } from "./BarChart";

const BARS = [
  { start: "2026-10-01T00:00:00Z", count: 3 },
  { start: "2026-10-02T00:00:00Z", count: 10 },
  { start: "2026-10-03T00:00:00Z", count: 0 },
];

describe("bar chart", () => {
  it("rounds the axis top to 1, 2 or 5 times a power of ten", () => {
    expect([0, 1, 3, 7, 10, 11, 130, 2400].map(niceMax)).toEqual([1, 1, 5, 10, 10, 20, 200, 5000]);
  });

  it("draws one bar per bucket, as tall as its count against the axis from zero", () => {
    const { container } = render(<BarChart bars={BARS} unit="day" />);

    const bars = [...container.querySelectorAll("rect")];
    expect(bars).toHaveLength(3);
    const heights = bars.map((bar) => Number(bar.getAttribute("height")));
    expect(heights[1]! / heights[0]!).toBeCloseTo(10 / 3);
    expect(heights[2]).toBe(0);
    expect(screen.getByRole("img", { name: /Events per day: 13 in total/ })).toBeInTheDocument();
  });

  it("labels a day bucket with its date only, and an hour bucket with the time", () => {
    const day = render(<BarChart bars={BARS} unit="day" />);
    const dayLabels = [...day.container.querySelectorAll("text")].map((node) => node.textContent);
    expect(dayLabels.some((text) => /\d[:.]\d\d[:.]\d\d/.test(text))).toBe(false);
    day.unmount();

    const hour = render(<BarChart bars={BARS} unit="hour" />);
    const hourLabels = [...hour.container.querySelectorAll("text")].map((node) => node.textContent);
    expect(hourLabels.some((text) => /\d[:.]\d\d[:.]\d\d/.test(text))).toBe(true);
  });

  it("writes large numbers with separators or in short form", async () => {
    const user = userEvent.setup();
    const { container } = render(<BarChart bars={[{ start: "2026-10-01T00:00:00Z", count: 2_562_183 }]} unit="day" />);

    expect(screen.getByRole("img", { name: /2,562,183 in total/ })).toBeInTheDocument();
    expect([...container.querySelectorAll("text")].map((node) => node.textContent)).toContain("5 M");
    await user.click(screen.getByText("Show the numbers as a table"));
    expect(screen.getAllByText("2,562,183").length).toBeGreaterThan(0);
  });

  it("gives the same numbers in a table", async () => {
    const user = userEvent.setup();
    render(<BarChart bars={BARS} unit="day" />);

    await user.click(screen.getByText("Show the numbers as a table"));

    const table = screen.getByRole("table", { name: "Events per day" });
    expect(within(table).getAllByRole("row")).toHaveLength(4);
    expect(within(table).getByText("10")).toBeVisible();
  });

  it("says so when there is nothing to draw", () => {
    render(<BarChart bars={[]} unit="hour" />);

    expect(screen.getByRole("img", { name: "Events per hour: none" })).toBeInTheDocument();
  });

  it("places bars by time so quiet days keep their width", () => {
    const { container } = render(
      <BarChart
        bars={[
          { start: "2026-10-01T00:00:00Z", count: 1 },
          { start: "2026-10-08T00:00:00Z", count: 1 },
        ]}
        unit="day"
      />,
    );
    const rects = [...container.querySelectorAll("rect")];
    const xs = rects.map((r) => Number(r.getAttribute("x")));
    const w = Number(rects[0]!.getAttribute("width"));
    expect(xs[1]! - xs[0]!).toBeGreaterThan(w * 5);
  });

  it("spaces hour and week buckets by their own step, equal for consecutive buckets", () => {
    const xsOf = (unit: string, starts: string[]) => {
      const { container, unmount } = render(<BarChart bars={starts.map((start) => ({ start, count: 1 }))} unit={unit} />);
      const xs = [...container.querySelectorAll("rect")].map((r) => Number(r.getAttribute("x")));
      unmount();
      return xs;
    };
    const hours = xsOf("hour", ["2026-10-01T00:00:00Z", "2026-10-01T01:00:00Z", "2026-10-01T04:00:00Z"]);
    expect(hours[2]! - hours[1]!).toBeCloseTo(3 * (hours[1]! - hours[0]!));
    const weeks = xsOf("week", ["2026-09-28T00:00:00Z", "2026-10-05T00:00:00Z", "2026-10-12T00:00:00Z"]);
    expect(weeks[2]! - weeks[1]!).toBeCloseTo(weeks[1]! - weeks[0]!);
  });

  it("draws one bar and an empty list without dividing by zero, and falls back to the index for an unknown unit", () => {
    const one = render(<BarChart bars={[{ start: "2026-10-01T00:00:00Z", count: 4 }]} unit="day" />);
    const x = Number(one.container.querySelector("rect")!.getAttribute("x"));
    expect(Number.isFinite(x)).toBe(true);
    one.unmount();

    const none = render(<BarChart bars={[]} unit="day" />);
    expect(none.container.querySelectorAll("rect")).toHaveLength(0);
    none.unmount();

    const odd = render(
      <BarChart
        bars={[
          { start: "2026-10-01T00:00:00Z", count: 1 },
          { start: "2026-10-20T00:00:00Z", count: 1 },
          { start: "2026-10-21T00:00:00Z", count: 1 },
        ]}
        unit="month"
      />,
    );
    const xs = [...odd.container.querySelectorAll("rect")].map((r) => Number(r.getAttribute("x")));
    expect(xs[2]! - xs[1]!).toBeCloseTo(xs[1]! - xs[0]!);
  });
});
