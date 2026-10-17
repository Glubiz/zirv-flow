import { useEffect, useRef } from "react";

import { Button, SelectField } from "../../ui";
import {
  addChild,
  changeKind,
  childrenOf,
  defaultNode,
  getAt,
  kindOf,
  pointerOf,
  removeChild,
  samePath,
  updateAt,
  type Kind,
  type Node,
  type Path,
} from "./ast";
import { BuilderContext, useBuilder, type BuilderApi, type PlacedIssue } from "./builderContext";
import type { Catalog, EventTypes } from "./catalog";
import { ConsentEditor, EventEditor, IdentifierEditor, ListEditor, SegmentEditor, TraitEditor } from "./Conditions";
import { issueMessage } from "./labels";

const GROUP_LABELS: Record<string, string> = { all: "All of", any: "Any of", not: "Not" };

const TYPE_OPTIONS: readonly { kind: Kind; label: string }[] = [
  { kind: "all", label: "All of" },
  { kind: "any", label: "Any of" },
  { kind: "not", label: "Not" },
  { kind: "trait", label: "Field" },
  { kind: "event", label: "Event" },
  { kind: "list", label: "In a list" },
  { kind: "segment", label: "In a segment" },
  { kind: "consent", label: "Consent" },
  { kind: "identifier", label: "Has an identifier" },
];

/** Where a node sits, in words: "2.1" is the first child of the second child. Empty for the top. */
function whereIs(path: Path): string {
  return path
    .filter((token) => typeof token === "number" || token === "not")
    .map((token) => (typeof token === "number" ? String(token + 1) : "not"))
    .join(".");
}

function nameOf(node: Node, path: Path): string {
  const kind = kindOf(node);
  const base = GROUP_LABELS[kind] ?? "Condition";
  const where = whereIs(path);
  return where === "" ? base : `${base} ${where}`;
}

function claimed(kind: Kind, field: string): boolean {
  switch (kind) {
    case "trait":
      return field === "trait" || field === "op" || field === "value";
    case "event":
      return field === "event" || field === "window" || field.startsWith("where.") || field.startsWith("agg.");
    case "list":
    case "segment":
    case "identifier":
      return field === kind;
    case "consent":
      return field.startsWith("consent.");
    default:
      return false;
  }
}

interface Removal {
  group: Path;
  position: number;
}

interface NodeEditorProps {
  node: Node;
  path: Path;
  removal: Removal | null;
}

function NodeEditor({ node, path, removal }: NodeEditorProps) {
  const { readOnly, issues, remove, changeKind: change, add } = useBuilder();
  const kind = kindOf(node);
  const name = nameOf(node, path);
  const children = childrenOf(node);
  const loose = issues.filter((issue) => samePath(issue.path, path) && !claimed(kind, issue.field));
  const groupWord = kind === "all" ? "all" : "any";

  return (
    <fieldset className={children !== null || kind === "not" ? "node node-group" : "node"} data-node={pointerOf(path) || "/"} tabIndex={-1}>
      <legend className="node-legend">{name}</legend>
      <div className="node-head">
        <SelectField label="Type" value={kind} onChange={(event) => change(path, event.target.value)} disabled={readOnly}>
          {TYPE_OPTIONS.map((option) => (
            <option key={option.kind} value={option.kind}>
              {option.label}
            </option>
          ))}
        </SelectField>
        {removal !== null && !readOnly ? (
          <Button onClick={() => remove(removal.group, removal.position)}>
            Remove <span className="visually-hidden">{name}</span>
          </Button>
        ) : null}
      </div>
      {loose.map((issue) => (
        <p className="field-error" key={`${issue.pointer}:${issue.code}`}>
          {issueMessage(issue.code)}
        </p>
      ))}
      {children !== null ? (
        <div className="node-children">
          {children.length === 0 ? <p className="faint">Nothing here yet. {kind === "all" ? "Everyone matches." : "No one matches."}</p> : null}
          {children.map((child, at) => (
            <NodeEditor key={at} node={child} path={[...path, groupWord, at]} removal={{ group: path, position: at }} />
          ))}
          {readOnly ? null : (
            <div className="row">
              <Button onClick={() => add(path, defaultNode("trait"))}>
                Add condition <span className="visually-hidden">to {name}</span>
              </Button>
              <Button onClick={() => add(path, defaultNode("all"))}>
                Add group <span className="visually-hidden">to {name}</span>
              </Button>
            </div>
          )}
        </div>
      ) : null}
      {"not" in node ? (
        <div className="node-children">
          <NodeEditor node={node.not} path={[...path, "not"]} removal={null} />
        </div>
      ) : null}
      {"trait" in node ? <TraitEditor node={node} path={path} /> : null}
      {"event" in node ? <EventEditor node={node} path={path} /> : null}
      {"list" in node ? <ListEditor node={node} path={path} /> : null}
      {"segment" in node ? <SegmentEditor node={node} path={path} /> : null}
      {"consent" in node ? <ConsentEditor node={node} path={path} /> : null}
      {"identifier" in node ? <IdentifierEditor node={node} path={path} /> : null}
    </fieldset>
  );
}

interface BuilderProps {
  root: Node;
  readOnly: boolean;
  selfId: string | null;
  catalog: Catalog;
  eventTypes: EventTypes;
  issues: readonly PlacedIssue[];
  /** The tree after an edit, the node it touched, and whether the edit reshaped the tree below it. */
  onEdit: (next: Node, touched: Path, scope: "node" | "structure") => void;
}

/** Edits the definition tree in place: groups, conditions, and the pickers each condition needs. */
export function Builder({ root, readOnly, selfId, catalog, eventTypes, issues, onEdit }: BuilderProps) {
  const pending = useRef<string | null>(null);
  const container = useRef<HTMLDivElement>(null);

  // After an add, a remove or a change of type, focus lands on the node that took its place.
  useEffect(() => {
    if (pending.current === null) return;
    const wanted = pending.current;
    pending.current = null;
    const target = Array.from(container.current?.querySelectorAll<HTMLElement>("[data-node]") ?? []).find(
      (element) => element.dataset["node"] === wanted,
    );
    target?.focus();
  });

  const api: BuilderApi = {
    readOnly,
    selfId,
    catalog,
    eventTypes,
    issues,
    edit: (path, next) => onEdit(updateAt(root, path, () => next), path, "node"),
    add: (group, child) => {
      const parent = getAt(root, group);
      const list = parent ? childrenOf(parent) : null;
      if (parent === undefined || list === null) return;
      pending.current = pointerOf([...group, kindOf(parent), list.length]);
      onEdit(addChild(root, group, child), group, "structure");
    },
    remove: (group, position) => {
      pending.current = pointerOf(group) || "/";
      onEdit(removeChild(root, group, position), group, "structure");
    },
    changeKind: (path, kind) => {
      const current = getAt(root, path);
      const wanted = TYPE_OPTIONS.find((option) => option.kind === kind)?.kind;
      if (current === undefined || wanted === undefined) return;
      pending.current = pointerOf(path) || "/";
      onEdit(updateAt(root, path, () => changeKind(current, wanted)), path, "structure");
    },
  };

  return (
    <BuilderContext.Provider value={api}>
      <div ref={container} className="builder">
        <NodeEditor node={root} path={[]} removal={null} />
      </div>
    </BuilderContext.Provider>
  );
}
