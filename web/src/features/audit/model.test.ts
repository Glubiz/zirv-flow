import { describe, expect, it } from "vitest";

import { actionLabel, groupAreas, groupOf } from "./areas";
import { changesOf, compact, exportUrl, filtersFromParams, NO_FILTERS, objectHref, paramsFromFilters } from "./model";

describe("audit model", () => {
  it("maps areas to groups and leaves an unknown area in Other", () => {
    expect(groupOf("session")).toBe("Sign-in");
    expect(groupOf("membership")).toBe("Members and keys");
    expect(groupOf("never_seen")).toBe("Other");
    expect(groupAreas([{ area: "never_seen", actions: ["never_seen.x"] }]).map((entry) => entry.group)).toEqual(["Other"]);
  });

  it("derives a label from a code", () => {
    expect(actionLabel("flow.enrollment_retried")).toBe("Flow enrollment retried");
  });

  it("renders a pair as from -> to and nested values as text", () => {
    expect(compact({ from: "a", to: "b" })).toBe("a -> b");
    expect(compact({ a: { b: 1 }, c: [1, 2] })).toBe("a: b: 1, c: 1, 2");
    expect(changesOf({ new_role: "admin" })).toEqual([{ key: "new_role", label: "New role", value: "admin" }]);
    expect(changesOf(null)).toEqual([]);
  });

  it("round-trips filters through URL params", () => {
    const filters = { ...NO_FILTERS, action: "flow.", requestId: "r1" };
    expect(filtersFromParams(paramsFromFilters(filters, false))).toEqual(filters);
    expect(paramsFromFilters(NO_FILTERS, true).toString()).toBe("scope=instance");
  });

  it("builds instance and workspace export URLs", () => {
    expect(exportUrl("w", true, NO_FILTERS, "csv")).toBe("/api/v1/audit/export?format=csv");
    expect(exportUrl("w", false, { ...NO_FILTERS, actorId: "a" }, "ndjson")).toBe(
      "/api/v1/workspaces/w/audit/export?format=ndjson&actor_id=a",
    );
  });

  it("links flows but never persons or removed objects", () => {
    const base = { id: 1, action: "flow.published", actor_kind: "user", occurred_at: "", summary: {} };
    expect(objectHref("w", { ...base, object_kind: "flow", object_id: "f1" })).toBe("/w/w/flows/f1");
    expect(objectHref("w", { ...base, object_kind: "person", object_id: "p1" })).toBeNull();
    expect(objectHref("w", { ...base, action: "flow.deleted", object_kind: "flow", object_id: "f1" })).toBeNull();
  });

  it("links an archived object, which still has a page", () => {
    const entry = { id: 1, action: "flow.archived", actor_kind: "user", occurred_at: "", summary: {}, object_kind: "flow", object_id: "f1" };
    expect(objectHref("w", entry)).toBe("/w/w/flows/f1");
  });

  it("reads known summary keys in plain words and derives the label of an unknown one", () => {
    expect(changesOf({ view_pii: true, role: "admin", odd_key: 1 }).map((change) => change.label)).toEqual([
      "Personal data",
      "Role",
      "Odd key",
    ]);
  });
});
