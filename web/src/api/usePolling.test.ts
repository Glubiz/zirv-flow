import { renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { usePolling } from "./usePolling";

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("usePolling", () => {
  it("replaces a request that never settles after 5 skipped beats", () => {
    const refresh = vi.fn();
    renderHook(() => usePolling(true, true, refresh, 100));

    vi.advanceTimersByTime(500);
    expect(refresh).not.toHaveBeenCalled();
    vi.advanceTimersByTime(100);
    expect(refresh).toHaveBeenCalledTimes(1);
    // The count starts over after a refresh.
    vi.advanceTimersByTime(500);
    expect(refresh).toHaveBeenCalledTimes(1);
    vi.advanceTimersByTime(100);
    expect(refresh).toHaveBeenCalledTimes(2);
  });

  it("lets a slow request settle, then refreshes on the next beat", () => {
    const refresh = vi.fn();
    const { rerender } = renderHook(({ busy }) => usePolling(true, busy, refresh, 100), { initialProps: { busy: true } });

    vi.advanceTimersByTime(400);
    expect(refresh).not.toHaveBeenCalled();
    rerender({ busy: false });
    expect(refresh).not.toHaveBeenCalled();
    vi.advanceTimersByTime(100);
    expect(refresh).toHaveBeenCalledTimes(1);
  });

  it("starts the count over when the request settles in between", () => {
    const refresh = vi.fn();
    const { rerender } = renderHook(({ busy }) => usePolling(true, busy, refresh, 100), { initialProps: { busy: true } });

    vi.advanceTimersByTime(400);
    rerender({ busy: false });
    rerender({ busy: true });
    vi.advanceTimersByTime(400);
    expect(refresh).not.toHaveBeenCalled();
  });

  it("does not count beats while the tab is hidden", () => {
    const refresh = vi.fn();
    const hidden = vi.spyOn(document, "hidden", "get").mockReturnValue(true);
    renderHook(() => usePolling(true, true, refresh, 100));

    vi.advanceTimersByTime(1000);
    expect(refresh).not.toHaveBeenCalled();
    hidden.mockReturnValue(false);
    vi.advanceTimersByTime(500);
    expect(refresh).not.toHaveBeenCalled();
    vi.advanceTimersByTime(100);
    expect(refresh).toHaveBeenCalledTimes(1);
    hidden.mockRestore();
  });
});
