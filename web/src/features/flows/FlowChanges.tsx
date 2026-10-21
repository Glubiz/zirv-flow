import { DraftChanges, type DraftChange } from "../assistant";
import type { Names } from "./describe";
import type { Graph } from "./model";
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
}

function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

function valueWords(value: unknown): string {
  if (value === null || value === undefined) return "nothing";
  const text = typeof value === "string" ? value : JSON.stringify(value);
  return text.length > 60 ? `${text.slice(0, 57)}…` : text;
}

/** The steps, connections and settings a flow draft changes, in the canvas's words; every field stays behind a disclosure. */
export function FlowChanges({ changes, before, after, names }: Props) {
  if (changes.length === 0) return <p className="faint">Same as the saved draft.</p>;

  const steps: StepLine[] = [];
  const seen = new Set<string>();
  const connections = { added: 0, removed: 0, changed: 0 };
  const settings: DraftChange[] = [];
  for (const change of changes) {
    const parts = change.path.split("/").slice(1);
    if (parts[0] === "graph" && parts[1] === "nodes" && parts[2] !== undefined) {
      const id = parts[2];
      const whole = parts.length === 3;
      if (whole) {
        seen.add(id);
        steps.push({ kind: change.change === "added" ? "Added" : change.change === "removed" ? "Removed" : "Changed", id });
      } else if (!seen.has(id)) {
        seen.add(id);
        steps.push({ kind: "Changed", id });
      }
    } else if (parts[0] === "graph" && parts[1] === "edges") {
      connections[change.change] += 1;
    } else {
      settings.push(change);
    }
  }
  const touched = steps.length > 0;
  const wired = connections.added + connections.removed + connections.changed;

  return (
    <div className="stack-tight">
      <ul className="list-plain stack-tight" aria-label="What the draft changes">
        {steps.map((line) => {
          const was = stepTitle(before, names, line.id);
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
              <span className="strong">{line.kind}</span> · {text} <span className="faint mono fl-id">{line.id}</span>
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
            <span className="strong">Settings</span> · {change.path.split("/").slice(2).join(" › ") || "all of them"}:{" "}
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
