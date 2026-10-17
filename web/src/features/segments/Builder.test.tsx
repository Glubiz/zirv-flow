import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";

import { json, mockApi, type Call } from "../../test/mockApi";
import { renderRoute } from "../../test/renderFeature";
import {
  actor,
  catalogRoutes,
  detail,
  LIST_ID,
  OTHER_SEGMENT_ID,
  previewBody,
  previewed,
  refusal,
  SEGMENT_ID,
  WS,
} from "../../test/segmentsApi";
import { NewSegmentPage } from "./NewSegmentPage";
import { SegmentPage } from "./SegmentPage";

function routes(kind: "writer" | "reader" = "writer") {
  return {
    ...actor(kind),
    ...catalogRoutes(),
    [`POST ${WS}/segments/preview`]: () => json(previewBody()),
  };
}

function mountNew() {
  return renderRoute("/w/ws1/segments/new", "/w/:ws/segments/new", <NewSegmentPage />);
}

/** The definition last sent for a count, once the debounce has let it through. */
async function expectDefinition(calls: Call[], expected: unknown) {
  await waitFor(() => expect(previewed(calls).at(-1)).toEqual(expected), { timeout: 4000 });
}

async function addCondition(user: ReturnType<typeof userEvent.setup>, to = "All of") {
  await user.click(screen.getByRole("button", { name: `Add condition to ${to}` }));
}

describe("segment builder", () => {
  it("adds, nests and removes groups and conditions, and the definition follows", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mountNew();

    await screen.findByRole("group", { name: "All of" });
    await user.click(screen.getByRole("button", { name: "Add group to All of" }));
    await expectDefinition(calls, { v: 1, all: [{ all: [] }] });

    const inner = screen.getByRole("group", { name: "All of 1" });
    await user.selectOptions(within(inner).getByLabelText("Type"), "any");
    await user.click(screen.getByRole("button", { name: "Add condition to Any of 1" }));
    const condition = screen.getByRole("group", { name: "Condition 1.1" });
    await user.selectOptions(within(condition).getByLabelText("Field"), "city");
    await user.type(within(condition).getByLabelText("Value"), "Aarhus");
    await expectDefinition(calls, { v: 1, all: [{ any: [{ trait: "city", op: "eq", value: "Aarhus" }] }] });

    await user.click(screen.getByRole("button", { name: "Remove Condition 1.1" }));
    await expectDefinition(calls, { v: 1, all: [{ any: [] }] });
    await user.click(screen.getByRole("button", { name: "Remove Any of 1" }));
    await expectDefinition(calls, { v: 1, all: [] });
  });

  it("wraps exactly one condition in a not group", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mountNew();

    await screen.findByRole("group", { name: "All of" });
    await user.selectOptions(screen.getAllByLabelText("Type")[0]!, "not");
    const group = await screen.findByRole("group", { name: "Not" });

    expect(within(group).queryByRole("button", { name: /^Add (condition|group)/ })).not.toBeInTheDocument();
    const inner = within(group).getByRole("group", { name: /^Condition/ });
    await user.selectOptions(within(inner).getByLabelText("Field"), "vip");
    await user.selectOptions(within(inner).getByLabelText("Value"), "true");
    await expectDefinition(calls, { v: 1, not: { trait: "vip", op: "eq", value: true } });
  });

  it("offers the operators the validator accepts for the type of the field", async () => {
    mockApi(routes());
    const user = userEvent.setup();
    mountNew();

    await screen.findByRole("group", { name: "All of" });
    await addCondition(user);
    const condition = screen.getByRole("group", { name: "Condition 1" });
    const operators = () =>
      within(within(condition).getByLabelText("Comparison"))
        .getAllByRole("option")
        .map((option) => option.textContent);

    await user.selectOptions(within(condition).getByLabelText("Field"), "city");
    expect(operators()).toContain("starts with");
    expect(operators()).not.toContain("is greater than");

    await user.selectOptions(within(condition).getByLabelText("Field"), "age");
    expect(operators()).toContain("is greater than");
    expect(operators()).toContain("is between");
    expect(operators()).not.toContain("starts with");

    await user.selectOptions(within(condition).getByLabelText("Field"), "vip");
    expect(operators()).toEqual(["is", "has a value", "has no value"]);

    await user.selectOptions(within(condition).getByLabelText("Field"), "joined_at");
    expect(operators()).toContain("is within the last");
    expect(operators()).not.toContain("is");

    await user.selectOptions(within(condition).getByLabelText("Field"), "tags");
    expect(operators()).toEqual(["contains", "contains all of", "is empty", "has a value", "has no value"]);
  });

  it("fits the value input to the operator: a pair, a list, a duration and none", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mountNew();

    await screen.findByRole("group", { name: "All of" });
    await addCondition(user);
    const condition = screen.getByRole("group", { name: "Condition 1" });
    await user.selectOptions(within(condition).getByLabelText("Field"), "age");

    await user.selectOptions(within(condition).getByLabelText("Comparison"), "between");
    await user.type(within(condition).getByLabelText("Value, from"), "18");
    await user.type(within(condition).getByLabelText("Value, to"), "65");
    await expectDefinition(calls, { v: 1, all: [{ trait: "age", op: "between", value: [18, 65] }] });

    await user.selectOptions(within(condition).getByLabelText("Comparison"), "in");
    await user.type(within(condition).getByLabelText("Value, value 1"), "30");
    await user.click(within(condition).getByRole("button", { name: "Add a value to Value" }));
    await user.type(within(condition).getByLabelText("Value, value 2"), "40");
    await expectDefinition(calls, { v: 1, all: [{ trait: "age", op: "in", value: [30, 40] }] });

    await user.selectOptions(within(condition).getByLabelText("Field"), "joined_at");
    await user.selectOptions(within(condition).getByLabelText("Comparison"), "within_last");
    await user.click(within(condition).getByRole("button", { name: "Set Value to 7 days" }));
    await expectDefinition(calls, { v: 1, all: [{ trait: "joined_at", op: "within_last", value: "P7D" }] });

    await user.selectOptions(within(condition).getByLabelText("Comparison"), "exists");
    expect(within(condition).queryByLabelText("Value")).not.toBeInTheDocument();
    await expectDefinition(calls, { v: 1, all: [{ trait: "joined_at", op: "exists" }] });
  });

  it("builds an event condition with a window, a property filter and a summary", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mountNew();

    await screen.findByRole("group", { name: "All of" });
    await addCondition(user);
    const condition = screen.getByRole("group", { name: "Condition 1" });
    await user.selectOptions(within(condition).getByLabelText("Type"), "event");
    await user.selectOptions(within(condition).getByLabelText("Event"), "purchase");

    await user.selectOptions(within(condition).getByLabelText("Time window"), "last");
    await user.click(within(condition).getByRole("button", { name: "Set Window length to 30 days" }));

    await user.click(within(condition).getByRole("button", { name: "Add a property filter" }));
    await within(condition).findByRole("option", { name: "currency" });
    await user.selectOptions(within(condition).getByLabelText("Property of filter 1"), "currency");
    await user.type(within(condition).getByLabelText("Value of filter 1"), "EUR");

    await user.click(within(condition).getByRole("button", { name: "Add a summary" }));
    await user.selectOptions(within(condition).getByLabelText("Summary of the events"), "sum");
    await user.selectOptions(within(condition).getByLabelText("Property to summarise"), "amount");
    await user.type(within(condition).getByLabelText("Summary value"), "100");

    await expectDefinition(calls, {
      v: 1,
      all: [
        {
          event: "purchase",
          window: { last: "P30D" },
          where: [{ prop: "currency", op: "eq", value: "EUR" }],
          agg: { fn: "sum", prop: "amount", op: "gte", value: 100 },
        },
      ],
    });

    await user.click(within(condition).getByRole("button", { name: "Remove filter 1" }));
    await user.click(within(condition).getByRole("button", { name: "Remove the summary" }));
    await expectDefinition(calls, { v: 1, all: [{ event: "purchase", window: { last: "P30D" } }] });
  });

  it.each([
    ["a list", "list", "List", LIST_ID, { list: LIST_ID }],
    ["a segment", "segment", "Segment", OTHER_SEGMENT_ID, { segment: OTHER_SEGMENT_ID }],
    ["an identifier kind", "identifier", "Identifier kind", "phone", { identifier: "phone" }],
  ])("picks %s", async (_name, kind, label, choice, node) => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mountNew();

    await screen.findByRole("group", { name: "All of" });
    await addCondition(user);
    const condition = screen.getByRole("group", { name: "Condition 1" });
    await user.selectOptions(within(condition).getByLabelText("Type"), kind);
    await user.selectOptions(within(condition).getByLabelText(label), choice);
    await expectDefinition(calls, { v: 1, all: [node] });
  });

  it("picks a consent channel, topic and status", async () => {
    const calls = mockApi(routes());
    const user = userEvent.setup();
    mountNew();

    await screen.findByRole("group", { name: "All of" });
    await addCondition(user);
    const condition = screen.getByRole("group", { name: "Condition 1" });
    await user.selectOptions(within(condition).getByLabelText("Type"), "consent");
    await user.selectOptions(within(condition).getByLabelText("Topic"), "offers");
    await user.selectOptions(within(condition).getByLabelText("Status"), "unsubscribed");
    await expectDefinition(calls, { v: 1, all: [{ consent: { channel: "email", status: "unsubscribed", topic: "offers" } }] });
  });

  it("marks the field a refused definition names, shows an unmatched pointer at the top, and clears on edit", async () => {
    mockApi({
      ...routes(),
      [`POST ${WS}/segments`]: () =>
        refusal(422, "segment_invalid", [
          { pointer: "/all/0/value", code: "value_type_mismatch" },
          { pointer: "/all/9/trait", code: "unknown_trait" },
        ]),
    });
    const user = userEvent.setup();
    mountNew();

    await screen.findByRole("group", { name: "All of" });
    await user.type(screen.getByLabelText("Name"), "Aarhus people");
    await addCondition(user);
    const condition = screen.getByRole("group", { name: "Condition 1" });
    await user.selectOptions(within(condition).getByLabelText("Field"), "city");
    await user.type(within(condition).getByLabelText("Value"), "Aarhus");
    await user.click(screen.getByRole("button", { name: "Create segment" }));

    await waitFor(() => expect(within(condition).getByLabelText("Value")).toHaveAttribute("aria-invalid", "true"));
    const value = within(condition).getByLabelText("Value");
    expect(value).toHaveAccessibleDescription("The value does not fit the type.");
    const top = screen.getByText("Problems with the definition as a whole").closest("div");
    expect(top).toHaveTextContent("This field does not exist in the workspace.");

    await user.type(value, "x");
    expect(value).not.toHaveAttribute("aria-invalid");
    expect(top).toBeInTheDocument();
  });

  it("shows a saved segment read-only to someone who cannot write", async () => {
    mockApi({
      ...routes("reader"),
      [`GET ${WS}/segments/${SEGMENT_ID}`]: () => json(detail()),
    });
    renderRoute(`/w/ws1/segments/${SEGMENT_ID}`, "/w/:ws/segments/:id", <SegmentPage />);

    const value = await screen.findByLabelText("Value");
    expect(value).toBeDisabled();
    expect(value).toHaveValue("Aarhus");
    expect(screen.getByLabelText("Name")).toBeDisabled();
    expect(screen.getByLabelText("Field")).toBeDisabled();
    expect(screen.queryByRole("button", { name: /^Add (condition|group)/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Save" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Refresh now" })).not.toBeInTheDocument();
    expect(screen.getByText("You can look at this segment but not change it.")).toBeInTheDocument();
  });
});
