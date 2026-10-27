import { Pill } from "../../ui";
import { changeLabel, readablePath, valueText, type DraftChange } from "./labels";

function tone(change: DraftChange["change"]): "neutral" | "attention" | "error" {
  return change === "removed" ? "error" : change === "changed" ? "attention" : "neutral";
}

/** Whether the assistant repaired this field, or something inside it or around it. */
function wasRepaired(path: string, repaired: readonly string[]): boolean {
  return repaired.some((place) => place === path || place.startsWith(`${path}/`) || path.startsWith(`${place}/`));
}

/**
 * The draft against the saved version, field by field. The kind of change is a word, never only a
 * colour. A field the assistant repaired before the draft was shown is marked at its place.
 */
export function DraftChanges({ changes, repaired = [] }: { changes: DraftChange[]; repaired?: readonly string[] }) {
  if (changes.length === 0) {
    return <p className="faint">Same as the saved version.</p>;
  }
  return (
    <div className="table-scroll">
      <table className="table">
        <caption>What the draft changes</caption>
        <thead>
          <tr>
            <th scope="col">Field</th>
            <th scope="col">Change</th>
            <th scope="col">Before</th>
            <th scope="col">After</th>
          </tr>
        </thead>
        <tbody>
          {changes.map((change) => (
            <tr key={change.path}>
              <th scope="row" className="mono">
                {readablePath(change.path)}
              </th>
              <td>
                <Pill tone={tone(change.change)}>{changeLabel(change.change)}</Pill>
                {wasRepaired(change.path, repaired) ? <Pill tone="attention">Repaired</Pill> : null}
              </td>
              <td className="mono">{valueText(change.before)}</td>
              <td className="mono">{valueText(change.after)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
