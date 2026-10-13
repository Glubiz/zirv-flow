import { describe, expect, it } from "vitest";

import { formatAgo } from "./format";

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
