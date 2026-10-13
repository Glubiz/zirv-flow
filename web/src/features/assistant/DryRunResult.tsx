import { EmptyState, InlineError, Pill } from "../../ui";
import { dryRunReason, previewReason, valueText, type DryRun } from "./labels";

const MAX_COLUMNS = 5;

/**
 * What the draft does on real data. A mapping shows, per record, the result and the kinds and keys
 * it would fill, never the values; a manifest shows the first fields of the records the API gave.
 */
export function DryRunResult({ dryRun }: { dryRun: DryRun }) {
  if (dryRun.status === "unavailable") {
    return <p className="notice">{dryRunReason(dryRun.reason)}</p>;
  }
  if (dryRun.status === "failed") {
    const problem = dryRun.problem;
    return (
      <InlineError
        title="The dry run did not work"
        detail={`The source answered with ${problem?.title ?? "an error"}${problem?.code ? ` (${problem.code})` : ""}. The draft itself is valid.`}
      />
    );
  }
  if (dryRun.preview) {
    const preview = dryRun.preview;
    if (preview.records.length === 0) {
      return <EmptyState title="The source returned no records">There was nothing to try the draft on.</EmptyState>;
    }
    return (
      <div className="table-scroll">
        <table className="table">
          <caption>What the draft makes of the first records</caption>
          <thead>
            <tr>
              <th scope="col">Record</th>
              <th scope="col">Result</th>
              <th scope="col">Identifiers</th>
              <th scope="col">Fields</th>
            </tr>
          </thead>
          <tbody>
            {preview.records.map((record) => (
              <tr key={record.key}>
                <td className="strong">{record.key}</td>
                <td>
                  <Pill tone={record.result === "rejected" ? "error" : record.result === "deleted" ? "attention" : "neutral"}>
                    {record.result}
                  </Pill>
                  {record.errors.length > 0 ? (
                    <ul className="list-plain">
                      {record.errors.map((error) => (
                        <li key={`${error.field}-${error.reason}`}>
                          {error.field === "" ? "Record" : error.field}: {previewReason(error.reason)}
                        </li>
                      ))}
                    </ul>
                  ) : null}
                </td>
                <td>{record.identifiers.map((item) => item.kind).join(", ")}</td>
                <td>{Object.keys(record.traits).join(", ")}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    );
  }
  if (dryRun.sample) {
    const sample = dryRun.sample;
    if (sample.records.length === 0) {
      return <EmptyState title="The API returned no records">There was nothing to try the draft on.</EmptyState>;
    }
    const columns = Object.keys(sample.records[0] ?? {}).slice(0, MAX_COLUMNS);
    return (
      <div className="stack-tight">
        <div className="table-scroll">
          <table className="table">
            <caption>{`The first records of ${dryRun.stream ?? "the stream"}, as the API returned them`}</caption>
            <thead>
              <tr>
                {columns.map((column) => (
                  <th key={column} scope="col">
                    {column}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {sample.records.map((record, index) => (
                <tr key={index}>
                  {columns.map((column) => (
                    <td key={column}>{valueText(record[column])}</td>
                  ))}
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        {sample.truncated ? <p className="faint">The stream has more records than shown.</p> : null}
      </div>
    );
  }
  return <p className="faint">The dry run showed nothing.</p>;
}
