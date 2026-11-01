import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { DayBars } from "./DayBars";

const DAYS = [
  { date: "2026-10-01", value: 0 },
  { date: "2026-10-02", value: 140 },
  { date: "2026-10-03", value: 70 },
];

function draw() {
  return render(<DayBars days={DAYS} noun="people entered" label="Three days of entries" valueLabel="Entered" caption="Days are UTC." />);
}

describe("DayBars", () => {
  it("draws a bar per day scaled to the rounded-up top, with the first and last day and the caption", () => {
    const { container } = draw();

    const bars = [...container.querySelectorAll<HTMLElement>(".daybars-bar")];
    expect(bars.map((bar) => bar.style.height)).toEqual(["0%", "70%", "35%"]);
    const chart = screen.getByRole("img", { name: "Three days of entries" });
    expect(within(chart).getByText("200")).toBeInTheDocument();
    expect(within(chart).getByText("0")).toBeInTheDocument();
    expect(within(chart).getByText("1 Oct")).toBeInTheDocument();
    expect(within(chart).getByText("3 Oct")).toBeInTheDocument();
    expect(screen.getByText("Days are UTC.")).toBeInTheDocument();
  });

  it("names the day and the value in a tooltip while a bar is hovered", async () => {
    const { container } = draw();
    const user = userEvent.setup();
    expect(screen.queryByText(/2 Oct: /)).not.toBeInTheDocument();

    await user.hover(container.querySelectorAll(".daybars-col")[1] as HTMLElement);

    expect(screen.getByText("2 Oct: 140 people entered")).toBeInTheDocument();
    await user.unhover(container.querySelectorAll(".daybars-col")[1] as HTMLElement);
    expect(screen.queryByText(/2 Oct: /)).not.toBeInTheDocument();
  });

  it("lists every day in a table behind a disclosure", async () => {
    draw();
    const user = userEvent.setup();

    await user.click(screen.getByText("Show as table"));

    const table = screen.getByRole("table", { name: "Three days of entries" });
    expect(within(table).getByRole("columnheader", { name: "Entered" })).toBeInTheDocument();
    expect(within(table).getAllByRole("row")).toHaveLength(4);
    expect(within(table).getByRole("row", { name: "2 Oct 140" })).toBeInTheDocument();
  });

  it("keeps a scale of 1 when every day is zero", () => {
    render(<DayBars days={[{ date: "2026-10-01", value: 0 }]} noun="people entered" label="None" valueLabel="Entered" />);

    expect(within(screen.getByRole("img", { name: "None" })).getByText("1")).toBeInTheDocument();
  });
});
