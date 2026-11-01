import type { components } from "../../api";
import { childrenOf, getAt, place, pointerOf, type Node, type Path } from "./ast";

export type DraftMark = "added" | "changed";

type DraftChange = components["schemas"]["Change"];

const sameJson = (left: unknown, right: unknown) => JSON.stringify(left) === JSON.stringify(right);

/** A change that replaces a whole list of conditions: each drafted condition is new, or differs from the one it replaces. */
function markList(marks: Map<string, DraftMark>, group: Path, node: Node, change: DraftChange) {
  const children = childrenOf(node) ?? [];
  const before = Array.isArray(change.before) ? change.before : [];
  const key = "all" in node ? "all" : "any";
  children.forEach((child, at) => {
    const path = [...group, key, at];
    if (at >= before.length) marks.set(pointerOf(path), "added");
    else if (!sameJson(child, before[at])) marks.set(pointerOf(path), "changed");
  });
}

/**
 * The conditions of a drafted tree that the draft adds and the ones it changes, by JSON pointer of
 * the condition, for the builder to mark. A removed condition is not on the drafted tree.
 */
export function draftMarks(root: Node, changes: readonly DraftChange[]): Map<string, DraftMark> {
  const marks = new Map<string, DraftMark>();
  for (const change of changes) {
    if (change.change === "removed") continue;
    const where = place(root, change.path);
    if (where === null) continue;
    const node = getAt(root, where.path);
    if (node === undefined) continue;
    const wholeList = where.field === "" && (change.path.endsWith("/all") || change.path.endsWith("/any"));
    if (wholeList) {
      markList(marks, where.path, node, change);
      continue;
    }
    if (where.path.length === 0 && where.field === "") continue;
    const key = pointerOf(where.path);
    const whole = where.field === "" && change.change === "added";
    if (whole) marks.set(key, "added");
    else if (marks.get(key) !== "added") marks.set(key, "changed");
  }
  return marks;
}
