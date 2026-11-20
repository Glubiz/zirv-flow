import { describe, expect, it } from "vitest";

import { formatAgo, formatCount, formatDay, formatDuration } from "./format";

const NOW = Date.parse("2026-10-10T12:00:00Z");

describe("formatAgo", () => {
  it("says how long ago a moment was, in the largest whole unit", () => {
    expect(formatAgo("2026-10-10T11:59:30Z", NOW)).toBe("just now");
    expect(formatAgo("2026-10-10T11:59:00Z", NOW)).toBe("1 minute ago");
    expect(formatAgo("2026-10-10T11:15:00Z", NOW)).toBe("45 minutes ago");
    expect(formatAgo("2026-10-10T09:00:00Z", NOW)).toBe("3 hours ago");
    expect(formatAgo("2026-10-09T11:00:00Z", NOW)).toBe("1 day ago");
    expect(formatAgo("2026-10-01T12:00:00Z", NOW)).toBe("9 days ago");
  });

  it("reads a moment in the future as just now and leaves text that is not a time alone", () => {
    expect(formatAgo("2026-10-10T13:00:00Z", NOW)).toBe("just now");
    expect(formatAgo("soon", NOW)).toBe("soon");
  });
});

describe("formatCount", () => {
  it("separates thousands under 100,000 and abbreviates above", () => {
    expect(formatCount(0)).toBe("0");
    expect(formatCount(6945)).toBe("6,945");
    expect(formatCount(99_999)).toBe("99,999");
    expect(formatCount(120_000)).toBe("120 k");
    expect(formatCount(2_400_000)).toBe("2.4 M");
    expect(formatCount(3_000_000)).toBe("3 M");
  });

  it("never prints a figure that rounds out of its unit", () => {
    expect(formatCount(999_949)).toBe("999.9 k");
    expect(formatCount(999_950)).toBe("1 M");
    expect(formatCount(999_999)).toBe("1 M");
    expect(formatCount(1_000_000)).toBe("1 M");
    expect(formatCount(100_000)).toBe("100 k");
    expect(formatCount(-999_950)).toBe("-1 M");
    expect(formatCount(-999_999)).toBe("-1 M");
    expect(formatCount(-120_000)).toBe("-120 k");
  });
});

describe("formatDuration", () => {
  it("chooses the unit from the rounded figure", () => {
    expect([formatDuration(59.6), formatDuration(3599.9), formatDuration(86399)]).toEqual(["1 min", "1.0 h", "1.0 d"]);
  });

  it("holds at each boundary", () => {
    expect(formatDuration(null)).toBe("—");
    expect(formatDuration(0.5)).toBe("under 1 s");
    expect(formatDuration(59.4)).toBe("59 s");
    expect(formatDuration(60)).toBe("1 min");
    expect(formatDuration(3599.4)).toBe("1.0 h");
    expect(formatDuration(3569)).toBe("59 min");
    expect(formatDuration(3600)).toBe("1.0 h");
    expect(formatDuration(86399.9)).toBe("1.0 d");
    expect(formatDuration(86400)).toBe("1.0 d");
  });
});

describe("formatDay", () => {
  it("writes a calendar day as day and month, whatever the zone, and leaves other text alone", () => {
    expect(formatDay("2026-10-01")).toBe("1 Oct");
    expect(formatDay("2026-12-31")).toBe("31 Dec");
    expect(formatDay("later")).toBe("later");
  });
});
