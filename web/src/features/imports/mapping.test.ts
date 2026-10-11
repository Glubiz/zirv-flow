import { describe, expect, it } from "vitest";

import {
  draftProblems,
  draftsToMapping,
  guessDrafts,
  mappingToDrafts,
  newTransform,
  parseEntries,
  targetToApi,
  transformToApi,
  type ColumnDraft,
  type Mapping,
} from "./mapping";

function draft(column: string, change: Partial<ColumnDraft> = {}): ColumnDraft {
  return { column, target: { kind: "ignore" }, transforms: [], ...change };
}

describe("guessing a mapping", () => {
  it("takes the first email column and a phone column by their names", () => {
    const drafts = guessDrafts(["Name", "E-mail", "Backup email", "Mobile"]);

    expect(drafts.map((entry) => entry.target.kind)).toEqual(["ignore", "identifier", "ignore", "identifier"]);
    expect(drafts[1]?.target).toEqual({ kind: "identifier", identifier: "email", namespace: "" });
    expect(drafts[1]?.transforms.map((step) => step.op)).toEqual(["trim", "lower"]);
    expect(drafts[3]?.target).toEqual({ kind: "identifier", identifier: "phone", namespace: "" });
  });

  it("guesses nothing for columns with unremarkable names", () => {
    expect(guessDrafts(["a", "b"]).every((entry) => entry.target.kind === "ignore")).toBe(true);
  });
});

describe("turning the editor's state into a mapping", () => {
  it("keeps the columns that are not ignored, in file order, with their steps", () => {
    const drafts = [
      draft("Id", { target: { kind: "ignore" } }),
      draft("Email", {
        target: { kind: "identifier", identifier: "email", namespace: "" },
        transforms: [newTransform("trim"), newTransform("lower")],
      }),
      draft("Plan", { target: { kind: "trait", key: "plan" } }),
      draft("News", { target: { kind: "list", listId: "list-1" } }),
      draft("Crm", { target: { kind: "identifier", identifier: "external", namespace: " crm " } }),
    ];

    expect(draftsToMapping(drafts, "Id")).toEqual({
      key_column: "Id",
      columns: [
        {
          column: "Email",
          target: { type: "identifier", kind: "email" },
          transforms: [{ op: "trim" }, { op: "lower" }],
        },
        { column: "Plan", target: { type: "trait", key: "plan" }, transforms: [] },
        { column: "News", target: { type: "list", list_id: "list-1" }, transforms: [] },
        { column: "Crm", target: { type: "identifier", kind: "external:crm" }, transforms: [] },
      ],
    });
    expect(draftsToMapping(drafts, "").key_column).toBeNull();
  });

  it("round-trips a saved mapping through the editor", () => {
    const saved: Mapping = {
      key_column: "Id",
      columns: [
        { column: "Email", target: { type: "identifier", kind: "email" }, transforms: [{ op: "trim" }] },
        {
          column: "Joined",
          target: { type: "trait", key: "joined" },
          transforms: [
            { op: "timestamp", format: "%d/%m/%Y" },
            { op: "cast", to: "timestamp" },
          ],
        },
        {
          column: "Tags",
          target: { type: "trait", key: "tags" },
          transforms: [
            { op: "split", separator: ";" },
            { op: "value_map", map: { a: "b" } },
            { op: "constant", value: "x" },
            { op: "concat", columns: ["Email"], separator: "-" },
          ],
        },
        { column: "Crm", target: { type: "identifier", kind: "external:crm" }, transforms: [] },
      ],
    };

    const set = mappingToDrafts(["Id", "Email", "Joined", "Tags", "Crm", "Other"], saved);

    expect(set.keyColumn).toBe("Id");
    expect(set.repeated).toEqual([]);
    expect(set.drafts[5]?.target.kind).toBe("ignore");
    expect(draftsToMapping(set.drafts, set.keyColumn)).toEqual(saved);
  });

  it("notes a column the saved mapping has several rules for", () => {
    const saved: Mapping = {
      columns: [
        { column: "Email", target: { type: "identifier", kind: "email" } },
        { column: "Email", target: { type: "trait", key: "email_copy" } },
      ],
    };

    const set = mappingToDrafts(["Email"], saved);

    expect(set.repeated).toEqual(["Email"]);
    expect(set.drafts[0]?.target.kind).toBe("identifier");
  });
});

describe("the pieces", () => {
  it("leaves a target or a step out until it is complete", () => {
    expect(targetToApi({ kind: "identifier", identifier: "external", namespace: " " })).toBeNull();
    expect(targetToApi({ kind: "trait", key: "" })).toBeNull();
    expect(targetToApi({ kind: "list", listId: "" })).toBeNull();
    expect(transformToApi({ ...newTransform("concat"), columns: [] })).toBeNull();
    expect(transformToApi({ ...newTransform("value_map"), entries: "no equals sign" })).toBeNull();
    expect(transformToApi({ ...newTransform("timestamp"), format: " " })).toBeNull();
    expect(transformToApi({ ...newTransform("split"), separator: "" })).toBeNull();
    expect(transformToApi(newTransform("constant"))).toEqual({ op: "constant", value: "" });
  });

  it("reads replacements one per line", () => {
    expect(parseEntries("M=male\n F = female \n\nbroken\n=nothing\nk=a=b")).toEqual({
      M: "male",
      F: "female",
      k: "a=b",
    });
  });
});

describe("what stops a mapping from being saved", () => {
  const email = draft("Email", { target: { kind: "identifier", identifier: "email", namespace: "" } });

  it("wants a column that identifies a person", () => {
    expect(draftProblems([draft("Plan", { target: { kind: "trait", key: "plan" } })])).toEqual([
      "Choose at least one column that identifies a person, such as an email address.",
    ]);
    expect(draftProblems([email])).toEqual([]);
  });

  it("names an unfinished choice, an unfinished step and a field filled twice", () => {
    const unfinished = [
      email,
      draft("Crm", { target: { kind: "identifier", identifier: "external", namespace: "" } }),
      draft("Born", {
        target: { kind: "trait", key: "born" },
        transforms: [{ ...newTransform("timestamp"), format: "" }],
      }),
      draft("A", { target: { kind: "trait", key: "plan" } }),
      draft("B", { target: { kind: "trait", key: "plan" } }),
    ];

    expect(draftProblems(unfinished)).toEqual([
      "Finish the choice for Crm.",
      'Finish the "Read as a date or time" step for Born.',
      "Two columns fill the same field; choose one of them.",
    ]);
  });
});
