import { describe, expect, it } from "vitest";

import { contrastNote, cornersOf, isHex, readTokens, reachedTemplates, statusText, usersOf, type ThemeIssue, type ThemeUsedBy } from "./model";

const issue = (at: string, message?: string): ThemeIssue => ({ code: "low_contrast", severity: "warning", at, message });

describe("the corner styles", () => {
  it("map the three radii to a style and any other radius to none", () => {
    expect([0, 6, 40].map(cornersOf)).toEqual(["square", "rounded", "pill"]);
    expect(cornersOf(12)).toBeNull();
  });
});

describe("colours", () => {
  it("accepts #rgb and #rrggbb only", () => {
    expect(["#fff", "#FFFFFF", "#7a3e1d"].every(isHex)).toBe(true);
    expect(["fff", "#ffff", "#12345g", "red", ""].some(isHex)).toBe(false);
  });
});

describe("contrast notes", () => {
  it("name the pair and the ratio asked for", () => {
    expect(contrastNote(issue("/theme/button_text", "contrast 4.20:1, WCAG AA asks 4.5:1"))).toBe("Button text on Button is 4.2:1. Aim for 4.5:1.");
    expect(contrastNote(issue("/theme/primary", "contrast 2.10:1, WCAG AA asks 3:1"))).toBe("Primary on Background is 2.1:1. Aim for 3:1.");
  });
});

describe("a theme's words", () => {
  const theme = { id: "t", name: "Warm", current_version: null, archived_at: null, is_default: false, created_at: "", updated_at: "" };

  it("says whether it is published", () => {
    expect(statusText(theme)).toBe("Not published");
    expect(statusText({ ...theme, current_version: 3 })).toBe("Draft · v3 live");
    expect(statusText({ ...theme, archived_at: "2026-10-01T00:00:00Z" })).toBe("Archived");
  });

  it("fills in any token the server left out and keeps web_font_url as stored", () => {
    const tokens = readTokens({ primary: "#111111", web_font_url: "https://fonts.example/x.css" });
    expect(tokens.primary).toBe("#111111");
    expect(tokens.content_width).toBe(600);
    expect(tokens.web_font_url).toBe("https://fonts.example/x.css");
  });
});

describe("where a theme is used", () => {
  const used: ThemeUsedBy = {
    named_templates: [{ id: "a", name: "A", in_draft: true, in_current: false }],
    default_templates: [
      { id: "a", name: "A", in_draft: true, in_current: false },
      { id: "b", name: "B", in_draft: false, in_current: true },
    ],
    active_flows: [{ flow_id: "f", flow_name: "Flow", flow_state: "published", template_ids: ["a"] }],
  };

  it("links templates and flows, kinds apart", () => {
    expect(usersOf("ws1", used).map((user) => [user.kind, user.href])).toEqual([
      ["named", "/w/ws1/templates/a"],
      ["default", "/w/ws1/templates/a"],
      ["default", "/w/ws1/templates/b"],
      ["flow", "/w/ws1/flows/f"],
    ]);
  });

  it("counts a template that is on both lists once for a publish", () => {
    expect(reachedTemplates(used).map((template) => template.id)).toEqual(["a", "b"]);
  });
});
