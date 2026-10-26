import { describe, expect, it } from "vitest";

import {
  checksLabel,
  copyNames,
  listParams,
  NO_FILTERS,
  readFilters,
  statusCounts,
  statusKey,
  themeLabel,
  usedLabel,
  writeFilters,
  type TemplateListItem,
} from "./library";
import { listItem } from "../../test/templatesApi";

function item(overrides: Record<string, unknown> = {}): TemplateListItem {
  return listItem(overrides) as TemplateListItem;
}

describe("the library filters in the URL", () => {
  it("round-trips and leaves defaults out", () => {
    const filters = { ...NO_FILTERS, q: "sale", status: "changes" as const, sort: "name" as const, view: "list" as const };

    const params = writeFilters(filters);

    expect(params.toString()).toBe("q=sale&status=changes&sort=name&view=list");
    expect(readFilters(params)).toEqual(filters);
    expect(writeFilters(NO_FILTERS).toString()).toBe("");
  });

  it("reads a value it does not know as the default", () => {
    const filters = readFilters(new URLSearchParams("status=old&channel=sms&sort=popular&view=grid"));

    expect(filters).toEqual(NO_FILTERS);
  });

  it("sends only what narrows the list", () => {
    expect(listParams(NO_FILTERS)).toEqual({});
    expect(listParams({ ...NO_FILTERS, q: "a", channel: "email", theme: "none", sort: "most_used" })).toEqual({
      q: "a",
      channel: "email",
      theme: "none",
      sort: "most_used",
    });
  });
});

describe("what a template shows", () => {
  it("tells live, changed, draft and archived apart", () => {
    expect(statusKey(item({ current_version: 2 }))).toBe("live");
    expect(statusKey(item({ current_version: 2, draft_changed: true }))).toBe("changes");
    expect(statusKey(item({ current_version: null }))).toBe("draft");
    expect(statusKey(item({ archived_at: "2026-10-02T00:00:00Z", current_version: 2 }))).toBe("archived");
  });

  it("counts the chips, with archived left out of All", () => {
    const counts = statusCounts([
      item({ current_version: 1 }),
      item({ current_version: 1, draft_changed: true }),
      item({}),
      item({ archived_at: "2026-10-02T00:00:00Z" }),
    ]);

    expect(counts).toEqual({ "": 3, live: 1, changes: 1, draft: 1, archived: 1 });
  });

  it("words the checks, the flows and the theme", () => {
    expect(checksLabel(item({ checks: null })).text).toBe("Not checked");
    expect(checksLabel(item({ checks: { blocking: 0, warnings: 0 } })).text).toBe("No issues");
    expect(checksLabel(item({ checks: { blocking: 0, warnings: 2 } }))).toEqual({ text: "2 warnings", tone: "attention" });
    expect(checksLabel(item({ checks: { blocking: 1, warnings: 2 } }))).toEqual({ text: "1 blocking issue", tone: "error" });
    expect(usedLabel(item({ active_flows: 0 }))).toBe("Not used by a flow");
    expect(usedLabel(item({ active_flows: 1 }))).toBe("Used in 1 flow");
    expect(usedLabel(item({ active_flows: 3 }))).toBe("Used in 3 flows");
    expect(themeLabel(item({ theme: "default" }))).toBe("Workspace default");
    expect(themeLabel(item({ theme: "none" }))).toBe("No theme");
    expect(themeLabel(item({ theme: { id: "t1", name: "Brand" } }))).toBe("Brand");
  });
});

describe("copy names", () => {
  it("add a number after the first, within the 100 characters a name may have", () => {
    expect(copyNames("Welcome", 3)).toEqual(["Copy of Welcome", "Copy of Welcome 2", "Copy of Welcome 3"]);
    const long = copyNames("x".repeat(100), 2);
    expect(long.every((name) => name.length <= 100)).toBe(true);
    expect(long[1]?.endsWith(" 2")).toBe(true);
  });
});
