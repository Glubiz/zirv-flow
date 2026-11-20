import { describe, expect, it } from "vitest";

import { mergeSaved, type CapDraft, type QuietDraft, type SettingsDraft } from "./model";

const cap = (key: number, change: Partial<CapDraft> = {}): CapDraft => ({ key, channel: "email", hours: "24", limit: "3", mode: "skip", ...change });
const quiet = (change: Partial<QuietDraft> = {}): QuietDraft => ({ start: "21:00", end: "08:00", days: ["mon", "tue"], ...change });
const draft = (caps: CapDraft[], quietHours: QuietDraft | null): SettingsDraft => ({ caps, quiet: quietHours });

describe("mergeSaved", () => {
  const sent = draft([cap(1)], quiet());

  it("is the saved settings when nothing was edited", () => {
    const saved = draft([cap(1, { limit: "4", hours: "48" })], quiet({ end: "07:00", days: ["mon"] }));
    expect(mergeSaved(sent, sent, saved)).toEqual(saved);
  });

  it("keeps a cap added during the save", () => {
    const current = draft([cap(1), cap(2, { limit: "9" })], quiet());
    const saved = draft([cap(1, { limit: "4" })], quiet());
    expect(mergeSaved(current, sent, saved).caps).toEqual(current.caps);
  });

  it("keeps a cap removed during the save", () => {
    const current = draft([], quiet());
    const saved = draft([cap(1, { limit: "4" })], quiet());
    expect(mergeSaved(current, sent, saved).caps).toEqual([]);
  });

  it("keeps quiet hours toggled off during the save", () => {
    const current = draft([cap(1)], null);
    expect(mergeSaved(current, sent, draft([cap(1)], quiet({ end: "07:00" }))).quiet).toBeNull();
  });

  it("keeps quiet hours toggled on during the save", () => {
    const current = draft([cap(1)], quiet({ start: "22:00" }));
    const without = draft([cap(1)], null);
    expect(mergeSaved(current, draft([cap(1)], null), without).quiet).toEqual(quiet({ start: "22:00" }));
  });

  it("keeps a quiet-hours time edited during the save and adopts the rest", () => {
    const current = draft([cap(1)], quiet({ start: "22:00" }));
    const saved = draft([cap(1)], quiet({ end: "07:00" }));
    expect(mergeSaved(current, sent, saved).quiet).toEqual(quiet({ start: "22:00", end: "07:00" }));
  });

  it("keeps quiet-hours days edited during the save", () => {
    const current = draft([cap(1)], quiet({ days: ["mon", "tue", "wed"] }));
    const saved = draft([cap(1)], quiet({ days: ["mon"], start: "20:00" }));
    expect(mergeSaved(current, sent, saved).quiet).toEqual(quiet({ days: ["mon", "tue", "wed"], start: "20:00" }));
  });

  it("adopts the saved value of a field edited and then edited back to the sent value", () => {
    const saved = draft([cap(1, { limit: "4" })], quiet());
    // The form holds the sent value again, so nothing differs from what was sent.
    expect(mergeSaved(draft([cap(1)], quiet()), sent, saved).caps[0]?.limit).toBe("4");
  });

  it("keeps a cap field edited during the save and adopts the others", () => {
    const current = draft([cap(1, { hours: "12" })], quiet());
    const saved = draft([cap(1, { limit: "4", hours: "24" })], quiet());
    expect(mergeSaved(current, sent, saved).caps).toEqual([cap(1, { hours: "12", limit: "4" })]);
  });
});
