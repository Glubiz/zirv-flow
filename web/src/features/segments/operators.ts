import type { components } from "../../api";

export type Op = components["schemas"]["SegmentOp"];

/** The value types a trait or an event property can have. */
export type ValueType = "string" | "number" | "integer" | "boolean" | "timestamp" | "date" | "string_list" | "object";

/** How an operator takes its operand. */
export type Shape = "none" | "scalar" | "list" | "pair" | "duration";

const STRING_OPS: readonly Op[] = ["eq", "neq", "in", "not_in", "contains", "starts_with", "exists", "not_exists"];
const NUMBER_OPS: readonly Op[] = [
  "eq",
  "neq",
  "gt",
  "gte",
  "lt",
  "lte",
  "between",
  "in",
  "not_in",
  "exists",
  "not_exists",
];
const BOOLEAN_OPS: readonly Op[] = ["eq", "exists", "not_exists"];
const TIME_OPS: readonly Op[] = [
  "before",
  "after",
  "between",
  "within_last",
  "more_than_ago",
  "within_next",
  "exists",
  "not_exists",
];
const LIST_OPS: readonly Op[] = ["contains", "contains_all", "is_empty", "exists", "not_exists"];
const OBJECT_OPS: readonly Op[] = ["exists", "not_exists"];

/**
 * The operators the validator accepts for a value type. This mirrors `ops_for` in
 * `crates/zirv-flow-segments/src/validate.rs`; the API's issues stay the backstop if the two drift.
 */
const OPERATORS: Record<ValueType, readonly Op[]> = {
  string: STRING_OPS,
  number: NUMBER_OPS,
  integer: NUMBER_OPS,
  boolean: BOOLEAN_OPS,
  timestamp: TIME_OPS,
  date: TIME_OPS,
  string_list: LIST_OPS,
  object: OBJECT_OPS,
};

const VALUE_TYPES: readonly string[] = [
  "string",
  "number",
  "integer",
  "boolean",
  "timestamp",
  "date",
  "string_list",
  "object",
];

export function asValueType(type: string | null | undefined): ValueType | null {
  return type !== null && type !== undefined && VALUE_TYPES.includes(type) ? (type as ValueType) : null;
}

const PRESENCE: readonly Op[] = ["exists", "not_exists"];

/** The operators for a type. An aggregate is never missing, so it takes no presence operators. */
export function operatorsFor(type: ValueType, aggregate = false): readonly Op[] {
  const all = OPERATORS[type];
  return aggregate ? all.filter((op) => !PRESENCE.includes(op)) : all;
}

export function shapeOf(type: ValueType, op: Op): Shape {
  switch (op) {
    case "exists":
    case "not_exists":
    case "is_empty":
      return "none";
    case "in":
    case "not_in":
      return "list";
    case "between":
      return "pair";
    case "within_last":
    case "more_than_ago":
    case "within_next":
      return "duration";
    case "contains":
    case "contains_all":
      return type === "string_list" ? "list" : "scalar";
    default:
      return "scalar";
  }
}

/** The type of one scalar operand: a string list's items are strings. */
export function operandType(type: ValueType): ValueType {
  return type === "string_list" ? "string" : type;
}

export const OP_LABELS: Record<Op, string> = {
  eq: "is",
  neq: "is not",
  in: "is one of",
  not_in: "is none of",
  contains: "contains",
  starts_with: "starts with",
  exists: "has a value",
  not_exists: "has no value",
  gt: "is greater than",
  gte: "is at least",
  lt: "is less than",
  lte: "is at most",
  between: "is between",
  before: "is before",
  after: "is after",
  within_last: "is within the last",
  more_than_ago: "is more than this long ago",
  within_next: "is within the next",
  contains_all: "contains all of",
  is_empty: "is empty",
};

export type AggFn = components["schemas"]["SegmentAggFn"];

export const AGG_FNS: readonly AggFn[] = ["count", "sum", "min", "max", "avg", "first_time", "last_time"];

export const AGG_LABELS: Record<AggFn, string> = {
  count: "Number of events",
  sum: "Sum of",
  min: "Smallest",
  max: "Largest",
  avg: "Average of",
  first_time: "Time of the first event",
  last_time: "Time of the latest event",
};

export function aggNeedsProperty(fn: AggFn): boolean {
  return fn === "sum" || fn === "min" || fn === "max" || fn === "avg";
}

/** What an aggregate compares: a number, or for the first and latest event a time. */
export function aggType(fn: AggFn): ValueType {
  return fn === "first_time" || fn === "last_time" ? "timestamp" : "number";
}

export const DURATION_PRESETS: readonly { label: string; value: string }[] = [
  { label: "1 day", value: "P1D" },
  { label: "7 days", value: "P7D" },
  { label: "30 days", value: "P30D" },
  { label: "90 days", value: "P90D" },
  { label: "1 year", value: "P365D" },
];
