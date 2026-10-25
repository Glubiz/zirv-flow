import { Pill } from "../../ui";
import { blockingCount, isBlocking, issueText, issueWhere, type TemplateIssue } from "./model";

/** The validation results of the draft as a list; each says whether it blocks publishing, in words. */
export function IssueList({
  issues,
  checked,
  onGo,
}: {
  issues: readonly TemplateIssue[];
  /** False until the server has checked the saved draft. */
  checked: boolean;
  onGo?: (where: string, issue: TemplateIssue) => void;
}) {
  const blocking = blockingCount(issues);
  if (!checked) {
    return (
      <section className="tp-issues" aria-label="Validation results">
        <h3 className="panel-title" role="status">
          Checking the draft
        </h3>
      </section>
    );
  }
  return (
    <section className="tp-issues" aria-label="Validation results">
      <h3 className="panel-title" role="status">
        {issues.length === 0
          ? "No issues"
          : `${issues.length} ${issues.length === 1 ? "issue" : "issues"}, ${blocking} blocking publishing`}
      </h3>
      {issues.length === 0 ? <p className="faint">The draft passes every check a publish makes.</p> : null}
      <ul className="list-plain">
        {issues.map((issue, index) => (
          <li key={`${issue.code}:${issue.at}:${index}`} className="tp-issue">
            <Pill tone={isBlocking(issue) ? "error" : "attention"}>{isBlocking(issue) ? "Blocks publishing" : "Warning"}</Pill>{" "}
            <span>{issueText(issue)}</span>{" "}
            {onGo ? (
              <button type="button" className="link-button" onClick={() => onGo(issueWhere(issue), issue)}>
                Go to {issueWhere(issue).toLowerCase()}
              </button>
            ) : (
              <span className="faint">{issueWhere(issue)}</span>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}

/** The issues of one field, under it. */
export function FieldIssues({ issues }: { issues: readonly TemplateIssue[] }) {
  if (issues.length === 0) return null;
  return (
    <ul className="list-plain" aria-label="Problems in this field">
      {issues.map((issue, index) => (
        <li key={`${issue.code}:${index}`} className={isBlocking(issue) ? "field-error" : "field-hint"}>
          {isBlocking(issue) ? "Blocks publishing: " : "Warning: "}
          {issueText(issue)}
        </li>
      ))}
    </ul>
  );
}
