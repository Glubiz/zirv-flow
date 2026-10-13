import { describe, expect, it } from "vitest";

import { formatMicros, fromMicros, toMicros } from "./money";

describe("money", () => {
  it("shows micro-units as an amount with two to four decimals", () => {
    expect(formatMicros(0)).toBe("0.00");
    expect(formatMicros(5_000_000)).toBe("5.00");
    expect(formatMicros(10_500)).toBe("0.0105");
    expect(formatMicros(21_000)).toBe("0.021");
    expect(formatMicros(1_250_000)).toBe("1.25");
  });

  it("turns a typed amount into micro-units and refuses anything else", () => {
    expect(toMicros("5")).toBe(5_000_000);
    expect(toMicros("0.5")).toBe(500_000);
    expect(toMicros("0")).toBe(0);
    expect(toMicros("")).toBeNull();
    expect(toMicros("-1")).toBeNull();
    expect(toMicros("abc")).toBeNull();
  });

  it("round-trips an amount field", () => {
    expect(fromMicros(2_500_000)).toBe("2.5");
    expect(toMicros(fromMicros(2_500_000))).toBe(2_500_000);
  });
});
