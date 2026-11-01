import { DraftChanges, type DraftChange } from "../assistant";
import type { Names } from "./describe";
import type { FlowNode, Graph } from "./model";
import { stepTitle } from "./SimulationSummary";

interface Props {
  changes: DraftChange[];
  /** The graph on the canvas, which the draft is read against. */
  before: Graph;
  /** The drafted graph, or null when it cannot be read. */
  after: Graph | null;
  names: Names;
}

interface StepLine {
  kind: "Added" | "Removed" | "Changed";
  id: string;
  /** A removed step as the diff reports it, so it keeps its title once the canvas no longer has it. */
  removed?: FlowNode;
}

function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

/** A token of a JSON pointer as the key it names (RFC 6901: ~1 is a slash, then ~0 is a tilde). */
function unescapePointer(token: string): string {
  return token.replaceAll("~1", "/").replaceAll("~0", "~");
}

function valueWords(value: unknown): string {
  if (value === null || value === undefined) return "nothing";
  const text = typeof value === "string" ? value : JSON.stringify(value);
  return text.length > 60 ? `${text.slice(0, 57)}…` : text;
}

/** The steps a draft adds and the steps it changes, by id, for the canvas to mark; a removed step is not on the drafted graph. */
export function draftMarks(changes: DraftChange[]): Map<string, "added" | "changed"> {
  const marks = new Map<string, "added" | "changed">();
  for (const change of changes) {
    const parts = change.path.split("/").slice(1).map(unescapePointer);
    if (parts[0] !== "graph" || parts[1] !== "nodes" || parts[2] === undefined || change.change === "removed") continue;
    const whole = parts.length === 3;
    if (whole && change.change === "added") marks.set(parts[2], "added");
    else if (marks.get(parts[2]) !== "added") marks.set(parts[2], "changed");
  }
  return marks;
}

/** The steps, connections and settings a flow draft changes, in the canvas's words; every field stays behind a disclosure. */
export function FlowChanges({ changes, before, after, names }: Props) {
  if (changes.length === 0) return <p className="faint">Same as the saved draft.</p>;

  const steps: StepLine[] = [];
  const seen = new Set<string>();
  /** One entry per connection, whatever number of its fields changed; a change of the whole connection wins. */
  const edges = new Map<string, DraftChange["change"]>();
  const settings: DraftChange[] = [];
  for (const change of changes) {
    const parts = change.path.split("/").slice(1).map(unescapePointer);
    if (parts[0] === "graph" && parts[1] === "nodes" && parts[2] !== undefined) {
      const id = parts[2];
      const whole = parts.length === 3;
      if (whole) {
        seen.add(id);
        const removed = change.change === "removed" && typeof change.before === "object" && change.before !== null;
        steps.push({
          kind: change.change === "added" ? "Added" : change.change === "removed" ? "Removed" : "Changed",
          id,
          ...(removed ? { removed: { ...(change.before as unknown as FlowNode), id } } : {}),
        });
      } else if (!seen.has(id)) {
        seen.add(id);
        steps.push({ kind: "Changed", id });
      }
    } else if (parts[0] === "graph" && parts[1] === "edges") {
      const id = parts[2];
      if (id !== undefined && (parts.length === 3 || !edges.has(id))) edges.set(id, parts.length === 3 ? change.change : "changed");
    } else if (parts[0] === "settings") {
      settings.push(change);
    }
  }
  const touched = steps.length > 0;
  const connections = { added: 0, removed: 0, changed: 0 };
  for (const kind of edges.values()) connections[kind] += 1;
  const wired = edges.size;

  return (
    <div className="stack-tight">
      <ul className="list-plain stack-tight" aria-label="What the draft changes">
        {steps.map((line) => {
          const was = stepTitle(line.removed ? { ...before, nodes: [line.removed] } : before, names, line.id);
          const now = stepTitle(after, names, line.id);
          const text =
            line.kind === "Added"
              ? now
              : line.kind === "Removed"
                ? was
                : was === now
                  ? now
                  : `${was} → ${now}`;
          return (
            <li key={`${line.kind}-${line.id}`}>
              <span className={`dock-mark dock-mark-${line.kind.toLowerCase()}`}>{line.kind}</span> · {text} <span className="faint mono fl-id">{line.id}</span>
            </li>
          );
        })}
        {wired > 0 ? (
          <li>
            <span className="strong">Connections</span> ·{" "}
            {[
              connections.added > 0 ? `${connections.added} added` : null,
              connections.removed > 0 ? `${connections.removed} removed` : null,
              connections.changed > 0 ? `${connections.changed} changed` : null,
            ]
              .filter(Boolean)
              .join(", ")}
          </li>
        ) : null}
        {settings.map((change) => (
          <li key={change.path}>
            <span className="strong">Settings</span> · {change.path.split("/").slice(2).map(unescapePointer).join(" › ") || "all of them"}:{" "}
            {valueWords(change.before)} → {valueWords(change.after)}
          </li>
        ))}
        {!touched && wired === 0 && settings.length === 0 ? <li>Nothing readable changed.</li> : null}
      </ul>
      <details>
        <summary>Show every field ({plural(changes.length, "change", "changes")})</summary>
        <DraftChanges changes={changes} />
      </details>
    </div>
  );
}
