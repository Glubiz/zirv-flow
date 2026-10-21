import type { ReactNode } from "react";

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
  /**
   * True when the oldest records were left out. The first record shown is then not the start of the trace, so its
   * cause is not the root's: the tree has a neutral root and every record names its own cause.
   */
  truncated?: boolean;
  /**
   * Marks records the reader was pointed to. Every row then has an element id from `rowId`, and the
   * `active` one is drawn stronger, so a link elsewhere can scroll to it and focus it.
   */
  cited?: { ids: ReadonlySet<string>; active: string | null; rowId: (recordId: string) => string };
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

interface Branch {
  record: TraceTreeRecord;
  /** What was decided after it, under the cause that started the trace, until the next branch. */
  children: TraceTreeRecord[];
}

function sameCause(left: TraceTreeRecord, right: TraceTreeRecord): boolean {
  return left.cause.kind === right.cause.kind && left.cause.id === right.cause.id;
}

/**
 * The tree of a trace. Its root is what started the trace, the cause of the first record. A record
 * with another cause, such as an operator's retry or skip, opens a branch; the records that follow
 * under the root's cause belong to that branch until the next one opens. Reading the tree top to
 * bottom is reading the records in the order they were written.
 */
function build(records: readonly TraceTreeRecord[]): { root: TraceTreeRecord | null; trunk: TraceTreeRecord[]; branches: Branch[] } {
  const root = records[0] ?? null;
  const trunk: TraceTreeRecord[] = [];
  const branches: Branch[] = [];
  for (const record of records) {
    if (root && !sameCause(record, root)) {
      branches.push({ record, children: [] });
    } else if (branches.length > 0) {
      branches[branches.length - 1]?.children.push(record);
    } else {
      trunk.push(record);
    }
  }
  return { root, trunk, branches };
}

function Row({ record, cited, children }: { record: TraceTreeRecord; cited: TraceTreeProps["cited"]; children: ReactNode }) {
  const isCited = cited?.ids.has(record.id) === true;
  const active = cited?.active === record.id;
  return (
    <li
      className={`trace-record${isCited ? " trace-record-cited" : ""}${active ? " trace-record-active" : ""}`}
      id={cited?.rowId(record.id)}
      tabIndex={cited ? -1 : undefined}
      aria-current={active ? "true" : undefined}
    >
      {children}
    </li>
  );
}

function Line({ record, describe, tag }: { record: TraceTreeRecord; describe?: TraceTreeProps["describe"]; tag?: boolean }) {
  const about = describe?.(record) ?? null;
  return (
    <>
      <span className="mono">{record.action}</span> <span className="mono">{record.outcome}</span>
      {about ? <span> {about}</span> : null}
      {record.reason ? <span className="muted"> · {record.reason}</span> : null}
      {record.attempt != null ? <span className="muted"> · attempt {record.attempt}</span> : null}
      {tag ? <span className="pill"> {CAUSES[record.cause.kind] ?? record.cause.kind}</span> : null}{" "}
      <time className="faint" dateTime={record.recorded_at}>
        {formatDateTime(record.recorded_at)}
      </time>
    </>
  );
}

/** The decisions of one trace as a tree: what started it, and under it what was decided, with each operator decision opening a branch. */
export function TraceTree({ records, label, describe, truncated = false, cited }: TraceTreeProps) {
  const { root, trunk, branches } = build(records);
  if (!root) return null;
  if (truncated) {
    return (
      <ul className="trace-tree" aria-label={label}>
        <li>
          <span className="strong">Earlier decisions are not shown</span>
          <ul className="trace-tree">
            {records.map((record) => (
              <Row key={record.id} record={record} cited={cited}>
                <Line record={record} describe={describe} tag />
              </Row>
            ))}
          </ul>
        </li>
      </ul>
    );
  }
  return (
    <ul className="trace-tree" aria-label={label}>
      <li>
        <span className="strong">{CAUSES[root.cause.kind] ?? root.cause.kind}</span> <span className="faint mono">{root.cause.kind}</span>
        <ul className="trace-tree">
          {trunk.map((record) => (
            <Row key={record.id} record={record} cited={cited}>
              <Line record={record} describe={describe} />
            </Row>
          ))}
          {branches.map((branch) => (
            <Row key={branch.record.id} record={branch.record} cited={cited}>
              <Line record={branch.record} describe={describe} tag />
              {branch.children.length > 0 ? (
                <ul className="trace-tree">
                  {branch.children.map((record) => (
                    <Row key={record.id} record={record} cited={cited}>
                      <Line record={record} describe={describe} />
                    </Row>
                  ))}
                </ul>
              ) : null}
            </Row>
          ))}
        </ul>
      </li>
    </ul>
  );
}
