import { describe, expect, it } from "vitest";

import { accessFor, type Member } from "./useWorkspaceAccess";

function member(role: string, viewPii = false): Member {
  return { user_id: "u1", email: "a@example.test", role, added_at: "2026-10-01T09:00:00Z", view_pii: viewPii, owner: false };
}

describe("accessFor", () => {
  it("gives the owner everything without a membership", () => {
    expect(accessFor([], "u1", true)).toEqual({
      role: null,
      isOwner: true,
      canEdit: true,
      canPublish: true,
      canAdmin: true,
      canViewPii: true,
    });
  });

  it("walks the lattice one role at a time", () => {
    const flags = (role: string) => {
      const { canEdit, canPublish, canAdmin } = accessFor([member(role)], "u1", false);
      return [canEdit, canPublish, canAdmin];
    };
    expect(flags("admin")).toEqual([true, true, true]);
    expect(flags("publisher")).toEqual([true, true, false]);
    expect(flags("editor")).toEqual([true, false, false]);
    expect(flags("viewer")).toEqual([false, false, false]);
  });

  it("always shows personal data to an admin and otherwise follows view_pii", () => {
    expect(accessFor([member("admin")], "u1", false).canViewPii).toBe(true);
    expect(accessFor([member("publisher")], "u1", false).canViewPii).toBe(false);
    expect(accessFor([member("publisher", true)], "u1", false).canViewPii).toBe(true);
    expect(accessFor([member("editor", true)], "u1", false).canViewPii).toBe(true);
    expect(accessFor([member("viewer", true)], "u1", false).canViewPii).toBe(true);
    expect(accessFor([member("viewer")], "u1", false).canViewPii).toBe(false);
  });

  it("treats someone who is not a member as having no role", () => {
    const access = accessFor([member("admin")], "other", false);
    expect(access.role).toBeNull();
    expect(access.canEdit).toBe(false);
  });

  it("reads an unknown role as a viewer, not as no role", () => {
    const access = accessFor([member("auditor")], "u1", false);
    expect(access.role).toBe("viewer");
    expect(access.canEdit).toBe(false);
    expect(access.canAdmin).toBe(false);
  });
});
