import { describe, expect, it } from "vitest";

import { formatWindow, monthsToDays, toDays, wholeMonths } from "./retentionUnits";

describe("retention units", () => {
  it("counts a month as 365/12 days, rounded to the nearest day", () => {
    expect(monthsToDays(13)).toBe(395);
    expect(monthsToDays(25)).toBe(760);
    expect(monthsToDays(24)).toBe(730);
  });

  it("shows a window in the largest whole unit", () => {
    expect(formatWindow(395)).toBe("13 months");
    expect(formatWindow(760)).toBe("25 months");
    expect(formatWindow(730)).toBe("2 years");
    expect(formatWindow(365)).toBe("1 year");
    expect(formatWindow(30)).toBe("30 days");
    expect(formatWindow(14)).toBe("14 days");
    expect(formatWindow(1)).toBe("1 day");
    expect(formatWindow(400)).toBe("400 days");
  });

  it("finds a whole number of months only when the days are exactly that", () => {
    expect(wholeMonths(395)).toBe(13);
    expect(wholeMonths(396)).toBeNull();
    expect(wholeMonths(30)).toBeNull();
  });

  it("converts the dialog's amount and unit to days", () => {
    expect(toDays(13, "months")).toBe(395);
    expect(toDays(45, "days")).toBe(45);
  });
});
