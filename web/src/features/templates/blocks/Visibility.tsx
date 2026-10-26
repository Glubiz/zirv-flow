import { Component, type ReactNode } from "react";

import { ConditionEditor, type ConditionNode } from "../../segments";
import { FieldIssues } from "../IssueList";
import type { TemplateIssue } from "../model";
import { Button } from "../../../ui";
import { isEditableCondition, type VisibleIf } from "./document";

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
  const conditional = value != null;
  const shown = isEditableCondition(value);
  const own = issues.filter((issue) => conditionPointer(issue) !== null);
  const whole = own.filter((issue) => conditionPointer(issue) === "");
  const inside = own.flatMap((issue) => {
    const pointer = conditionPointer(issue);
    return pointer ? [{ pointer, code: issue.code }] : [];
  });
  const unshown = (
    <div className="stack">
      <p className="notice" role="status">
        This condition can't be shown here.
      </p>
      {readOnly ? null : (
        <div>
          <Button onClick={() => onChange(EMPTY)}>Replace with a new condition</Button>
        </div>
      )}
    </div>
  );
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
      {conditional && shown ? (
        <ConditionGuard value={value} fallback={unshown}>
          <ConditionEditor workspaceId={workspaceId} value={value} onChange={onChange} readOnly={readOnly} issues={inside} />
        </ConditionGuard>
      ) : null}
      {conditional && !shown ? unshown : null}
    </div>
  );
}

/** Keeps a shape the check missed from taking the editor down: it shows the fallback until the condition changes. */
class ConditionGuard extends Component<{ value: unknown; fallback: ReactNode; children: ReactNode }, { failed: boolean; seen: unknown }> {
  override state = { failed: false, seen: this.props.value };

  static getDerivedStateFromError() {
    return { failed: true };
  }

  static getDerivedStateFromProps(props: { value: unknown }, state: { failed: boolean; seen: unknown }) {
    return props.value === state.seen ? null : { failed: false, seen: props.value };
  }

  override render() {
    return this.state.failed ? this.props.fallback : this.props.children;
  }
}
