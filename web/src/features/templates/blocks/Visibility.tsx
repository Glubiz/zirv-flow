import { ConditionEditor, type ConditionNode } from "../../segments";
import { FieldIssues } from "../IssueList";
import type { TemplateIssue } from "../model";
import type { VisibleIf } from "./document";

interface Props {
  workspaceId: string;
  value: VisibleIf | undefined;
  onChange: (next: VisibleIf | undefined) => void;
  readOnly: boolean;
  /** The issues on the block or section this belongs to. */
  issues: readonly TemplateIssue[];
}

/** A condition with nothing in it yet: it matches everyone until a rule is added. */
const EMPTY: ConditionNode = { all: [] };

/** The pointer of the condition itself inside the issues' `at`, or null for an issue about something else. */
function conditionPointer(issue: TemplateIssue): string | null {
  const at = issue.at.split("/visible_if");
  return at.length < 2 ? null : at.slice(1).join("/visible_if");
}

/** The "Show: Always | Only when" row of a block or section, with the condition below it. */
export function Visibility({ workspaceId, value, onChange, readOnly, issues }: Props) {
  const conditional = value !== undefined;
  const own = issues.filter((issue) => conditionPointer(issue) !== null);
  const whole = own.filter((issue) => conditionPointer(issue) === "");
  const inside = own.flatMap((issue) => {
    const pointer = conditionPointer(issue);
    return pointer ? [{ pointer, code: issue.code }] : [];
  });
  return (
    <div className="stack" role="group" aria-label="Visibility">
      <div className="row">
        <span className="field-label" id="be-show-label">
          Show:
        </span>
        <div className="seg-tabs" role="group" aria-labelledby="be-show-label">
          <button type="button" className={conditional ? "seg" : "seg seg-on"} aria-pressed={!conditional} disabled={readOnly} onClick={() => onChange(undefined)}>
            Always
          </button>
          <button type="button" className={conditional ? "seg seg-on" : "seg"} aria-pressed={conditional} disabled={readOnly} onClick={() => onChange(value ?? EMPTY)}>
            Only when
          </button>
        </div>
      </div>
      <FieldIssues issues={whole} />
      {conditional ? <ConditionEditor workspaceId={workspaceId} value={value} onChange={onChange} readOnly={readOnly} issues={inside} /> : null}
    </div>
  );
}
