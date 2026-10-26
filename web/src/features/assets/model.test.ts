import { describe, expect, it } from "vitest";

import { ApiError } from "../../api";
import { formatBytes, shortHash, typeLabel, uploadFailure } from "./model";

function refusal(code: string | null) {
  return new ApiError({ title: "Refused", detail: "detail", status: 415, traceId: "t1", requestId: null, code });
}

describe("asset model", () => {
  it("writes byte counts in the largest fitting unit", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(38 * 1024)).toBe("38 KB");
    expect(formatBytes(1.5 * 1024 * 1024)).toBe("1.5 MB");
    expect(formatBytes(1024 ** 3)).toBe("1 GB");
  });

  it("names types and shortens hashes", () => {
    expect(typeLabel("image/png")).toBe("PNG");
    expect(typeLabel("image/webp")).toBe("WebP");
    expect(shortHash("9f2c4e1b".repeat(8))).toBe("9f2c4e1b…4e1b");
  });

  it("keeps the server's own words for a code it has no message for", () => {
    expect(uploadFailure(refusal("something_else"), 5)).toEqual({ title: "Refused", detail: "detail", traceId: "t1" });
    expect(uploadFailure(refusal(null), 5).title).toBe("Refused");
    expect(uploadFailure(new Error("offline"), 5).detail).toBe("offline");
  });
});
