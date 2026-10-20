import { formatDateTime } from "../../ui";

/** The part of a decision record the tree reads; the trace endpoint and an enrollment's path both fit it. */
export interface TraceTreeRecord {
  id: string;
  recorded_at: string;
  action: string;
  outcome: string;
  cause: { kind: string; id: string };
  attempt?: number | null;
  reason?: string | null;
  subject_ids: Record<string, unknown>;
}

interface TraceTreeProps {
  /** Oldest first, as the API returns them. */
  records: readonly TraceTreeRecord[];
  /** The accessible name of the tree. */
  label: string;
  /** What a record is about in words, such as the step it names; shown after the action. */
  describe?: (record: TraceTreeRecord) => string | null;
}

const CAUSES: Record<string, string> = {
  event: "An event",
  trait_change: "A change of a field or segment",
  schedule: "A schedule",
  manual: "An operator",
  api: "An API call",
  definition_change: "A change of a definition",
  sync_run: "A sync run",
};

interface Group {
  key: string;
  kind: string;
  id: string;
  records: TraceTreeRecord[];
}

/** The records grouped under what caused them, in the order each cause first appears. */
function groupByCause(records: readonly TraceTreeRecord[]): Group[] {
  const groups: Group[] = [];
  for (const record of records) {
    const key = `${record.cause.kind}:${record.cause.id}`;
    let group = groups.find((candidate) => candidate.key === key);
    if (!group) {
      group = { key, kind: record.cause.kind, id: record.cause.id, records: [] };
      groups.push(group);
    }
    group.records.push(record);
  }
  return groups;
}

/** The decisions of one trace as a tree: each cause, and under it what was decided because of it. */
export function TraceTree({ records, label, describe }: TraceTreeProps) {
  return (
    <ul className="trace-tree" aria-label={label}>
      {groupByCause(records).map((group) => (
        <li key={group.key}>
          <span className="strong">{CAUSES[group.kind] ?? group.kind}</span> <span className="faint mono">{group.kind}</span>
          <ul className="trace-tree">
            {group.records.map((record) => {
              const about = describe?.(record) ?? null;
              return (
                <li key={record.id} className="trace-record">
                  <span className="mono">{record.action}</span> <span className="mono">{record.outcome}</span>
                  {about ? <span> {about}</span> : null}
                  {record.reason ? <span className="muted"> · {record.reason}</span> : null}
                  {record.attempt != null ? <span className="muted"> · attempt {record.attempt}</span> : null}{" "}
                  <time className="faint" dateTime={record.recorded_at}>
                    {formatDateTime(record.recorded_at)}
                  </time>
                </li>
              );
            })}
          </ul>
        </li>
      ))}
    </ul>
  );
}
