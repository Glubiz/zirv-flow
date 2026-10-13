import { Pill } from "../../ui";
import { changeLabel, readablePath, valueText, type DraftChange } from "./labels";

function tone(change: DraftChange["change"]): "neutral" | "attention" | "error" {
  return change === "removed" ? "error" : change === "changed" ? "attention" : "neutral";
}

/** The draft against the saved version, field by field. The kind of change is a word, never only a colour. */
export function DraftChanges({ changes }: { changes: DraftChange[] }) {
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
