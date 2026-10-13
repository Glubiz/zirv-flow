import { describe, expect, it } from "vitest";

import { newTransform } from "../../ui";
import {
  draftProblems,
  draftToMapping,
  emptyEvent,
  mappingToDraft,
  newProperty,
  newRule,
  startingDraft,
  type Mapping,
  type MappingDraft,
} from "./mapping";

const LIST = "11111111-1111-7111-8111-111111111111";

const SAVED: Mapping = {
  key_column: null,
  columns: [
    {
      column: "profile.email",
      target: { type: "identifier", kind: "email" },
      transforms: [{ op: "trim" }, { op: "lower" }],
    },
    { column: "profile.name", target: { type: "trait", key: "display_name" }, transforms: [] },
    { column: "flags.news", target: { type: "list", list_id: LIST }, transforms: [] },
    { column: "crm_id", target: { type: "identifier", kind: "external:crm" }, transforms: [] },
  ],
  event: {
    name: "order_placed",
    timestamp: { column: "order.at", transforms: [{ op: "timestamp", format: "%Y-%m-%d" }] },
    properties: [{ property: "sku", column: "order.items.0.sku", transforms: [] }],
  },
};

describe("a stream mapping draft", () => {
  it("starts with a guess at the email field and an event switched off", () => {
    const draft = startingDraft(["id", "Email address", "name"]);

    expect(draft.rules).toHaveLength(1);
    expect(draft.rules[0]?.column).toBe("Email address");
    expect(draft.rules[0]?.transforms.map((step) => step.op)).toEqual(["trim", "lower"]);
    expect(draft.event.enabled).toBe(false);
    expect(startingDraft(["id"]).rules[0]?.column).toBe("");
  });

  it("round trips a saved mapping with nested paths, a list, an external id and an event", () => {
    const draft = mappingToDraft(["profile"], SAVED);

    expect(draftToMapping(draft)).toEqual(SAVED);
    expect(draft.event.enabled).toBe(true);
    expect(draft.rules[3]?.target).toEqual({ kind: "identifier", identifier: "external", namespace: "crm" });
  });

  it("leaves the event out when it is switched off and leaves out unfinished rows and steps", () => {
    const draft = mappingToDraft([], SAVED);
    draft.event = { ...draft.event, enabled: false };
    draft.rules.push({ ...newRule(""), column: "   " });
    draft.rules.push({ ...newRule("x"), target: { kind: "trait", key: "" } });
    draft.rules[0]?.transforms.push(newTransform("concat"));

    const mapping = draftToMapping(draft);

    expect(mapping.event).toBeUndefined();
    expect(mapping.columns.map((rule) => rule.column)).toEqual(["profile.email", "profile.name", "flags.news", "crm_id"]);
    expect(mapping.columns[0]?.transforms).toEqual([{ op: "trim" }, { op: "lower" }]);
  });

  it("has no problems when it is complete and names each one in words when it is not", () => {
    const ok = mappingToDraft([], SAVED);
    expect(draftProblems(ok, true)).toEqual([]);

    const bad: MappingDraft = {
      rules: [
        { ...newRule("a"), target: { kind: "trait", key: "plan" } },
        { ...newRule("b"), target: { kind: "trait", key: "plan" } },
        { ...newRule("c"), target: { kind: "list", listId: "" } },
        { ...newRule(""), target: { kind: "list", listId: LIST }, transforms: [newTransform("concat")] },
      ],
      event: { ...emptyEvent(), enabled: true, properties: [newProperty()] },
    };
    const problems = draftProblems(bad, false);

    expect(problems).toContain("Choose at least one field that identifies a person, such as an email address.");
    expect(problems).toContain("Two rows fill the same field; choose one of them.");
    expect(problems).toContain("Finish the choice for c.");
    expect(problems).toContain("Name the field of every row.");
    expect(problems).toContain("An event needs a primary key on the stream, so a re-read never repeats it.");
    expect(problems).toContain("Name the event.");
    expect(problems).toContain("Give every property a name and a field.");
  });

  it("refuses duplicate property names and more than fifty properties", () => {
    const draft = mappingToDraft([], SAVED);
    const property = (name: string) => ({ ...newProperty(), property: name, column: "x" });
    draft.event.properties = [property("a"), property("a")];
    expect(draftProblems(draft, true)).toContain("Two properties have the same name.");

    draft.event.properties = Array.from({ length: 51 }, (_, at) => property(`p${at}`));
    expect(draftProblems(draft, true)).toContain("An event takes at most 50 properties.");
  });
});
