import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { FOLD_STORAGE_KEY, useSidebarFold } from "./useSidebarFold";

afterEach(() => vi.restoreAllMocks());

describe("sidebar folding", () => {
  it("remembers folding and unfolding across mounts", () => {
    const first = renderHook(() => useSidebarFold("/w/ws1"));
    expect(first.result.current.folded).toBe(false);
    act(() => first.result.current.toggle());
    expect(localStorage.getItem(FOLD_STORAGE_KEY)).toBe("true");
    first.unmount();
    const next = renderHook(() => useSidebarFold("/w/ws1"));
    expect(next.result.current.folded).toBe(true);
    act(() => next.result.current.toggle());
    expect(next.result.current.folded).toBe(false);
    expect(localStorage.getItem(FOLD_STORAGE_KEY)).toBe("false");
  });

  it("auto-folds the editor without overwriting the browser preference", () => {
    localStorage.setItem(FOLD_STORAGE_KEY, "false");
    const { result, rerender } = renderHook(({ path }) => useSidebarFold(path), { initialProps: { path: "/w/ws1" } });
    rerender({ path: "/w/ws1/flows/f1" });
    expect(result.current.folded).toBe(true);
    expect(localStorage.getItem(FOLD_STORAGE_KEY)).toBe("false");
    act(() => result.current.toggle());
    expect(result.current.folded).toBe(false);
    rerender({ path: "/w/ws1/flows/f2" });
    expect(result.current.folded).toBe(true);
    rerender({ path: "/w/ws1" });
    expect(result.current.folded).toBe(false);
    rerender({ path: "/w/ws1/flows/f1" });
    expect(result.current.folded).toBe(true);
  });

  it("keeps a stored folded preference when the editor is unfolded", () => {
    localStorage.setItem(FOLD_STORAGE_KEY, "true");
    const { result, rerender } = renderHook(({ path }) => useSidebarFold(path), { initialProps: { path: "/w/ws1/flows/f1" } });
    act(() => result.current.toggle());
    expect(result.current.folded).toBe(false);
    expect(localStorage.getItem(FOLD_STORAGE_KEY)).toBe("true");
    rerender({ path: "/w/ws1" });
    expect(result.current.folded).toBe(true);
  });

  it("still folds when reading and writing localStorage throw", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => { throw new Error("Storage blocked"); });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => { throw new Error("Storage blocked"); });
    const { result } = renderHook(() => useSidebarFold("/w/ws1"));
    expect(result.current.folded).toBe(false);
    act(() => result.current.toggle());
    expect(result.current.folded).toBe(true);
    act(() => result.current.toggle());
    expect(result.current.folded).toBe(false);
  });
});
