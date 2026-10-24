import type { DefinitionIssue } from "../../api";
import { Button, TextField } from "../../ui";
import { ConditionEditor, type BuilderMode, type ConditionNode } from "../segments";
import { ArrowDownIcon, ArrowUpIcon, BlockedIcon, ChevronDownIcon, ChevronUpIcon, LockIcon } from "./policyIcons";
import { MAX_RULE_NAME, ruleIssueMessage, type DraftRule } from "./policyModel";
import type { Dimension } from "./DimensionDialog";

const POLICY_MODE: BuilderMode = "policy";

interface RuleCardProps {
  workspaceId: string;
  rule: DraftRule;
  /** Zero-based position of the rule in the policy. */
  index: number;
  total: number;
  open: boolean;
  /** Everyone who can read the policy sees the rules; only someone who can write changes them. */
  readOnly: boolean;
  /** The condition in words, for the closed card. */
  summary: string;
  dimensions: readonly Dimension[];
  /** Codes of the problems with the rule itself, and the problems with its condition. */
  ruleIssues: readonly string[];
  conditionIssues: readonly DefinitionIssue[];
  /** The last check of a person was decided by this rule. */
  decides: boolean;
  onToggle: () => void;
  onMove: (by: -1 | 1) => void;
  onRename: (name: string) => void;
  onCondition: (condition: ConditionNode) => void;
  onDelete: () => void;
}

/** One rule of the policy: a numbered card with its name, what it checks, and the controls to move and edit it. */
export function RuleCard({
  workspaceId,
  rule,
  index,
  total,
  open,
  readOnly,
  summary,
  dimensions,
  ruleIssues,
  conditionIssues,
  decides,
  onToggle,
  onMove,
  onRename,
  onCondition,
  onDelete,
}: RuleCardProps) {
  const problems = ruleIssues.length + conditionIssues.length;
  const classes = ["rule", rule.builtin ? "rule-builtin" : "", decides ? "rule-decides" : "", problems > 0 ? "rule-problem" : ""]
    .filter(Boolean)
    .join(" ");
  const nameError = ruleIssues.includes("invalid_rule_name") ? ruleIssueMessage("invalid_rule_name") : undefined;
  const otherIssues = ruleIssues.filter((code) => code !== "invalid_rule_name");
  const bodyId = `rule-body-${rule.key}`;

  return (
    <li className={classes} data-rule={rule.key}>
      <div className="rule-head">
        <span className="rule-no" aria-hidden="true">
          {index + 1}
        </span>
        <div className="rule-title">
          <h3 className="rule-name">
            <span className="visually-hidden">{`Rule ${index + 1} of ${total}: `}</span>
            {rule.name}
          </h3>
          {rule.builtin ? (
            <span className="rule-tag">
              <LockIcon size={12} />
              Built in: can move, cannot be removed
            </span>
          ) : null}
          {decides ? (
            <span className="rule-tag rule-tag-decides">
              <BlockedIcon size={12} />
              Decided the last check
            </span>
          ) : null}
          {problems > 0 ? <span className="rule-tag rule-tag-error">{problems === 1 ? "1 problem" : `${problems} problems`}</span> : null}
        </div>
        {open ? null : (
          <div className="rule-summary">
            {rule.builtin ? (
              <span className="rule-cond">
                <span className="rule-cond-kind">Consent</span>What the person recorded for the message&apos;s channel and topic does not allow it
              </span>
            ) : (
              <span className="rule-cond">
                <span className="rule-cond-kind">When</span>
                {summary}
              </span>
            )}
          </div>
        )}
        <div role="group" aria-label={`Rule ${index + 1} actions`} className="rule-tools">
          {readOnly ? null : (
            <>
              <button
                type="button"
                className="icon-btn"
                data-move="up"
                aria-label={`Move ${rule.name} up`}
                disabled={index === 0}
                onClick={() => onMove(-1)}
              >
                <ArrowUpIcon />
              </button>
              <button
                type="button"
                className="icon-btn"
                data-move="down"
                aria-label={`Move ${rule.name} down`}
                disabled={index === total - 1}
                onClick={() => onMove(1)}
              >
                <ArrowDownIcon />
              </button>
            </>
          )}
          <button
            type="button"
            className="icon-btn"
            aria-expanded={open}
            aria-controls={bodyId}
            aria-label={
              rule.builtin
                ? open
                  ? `Hide how ${rule.name} decides`
                  : `Show how ${rule.name} decides`
                : `${open ? "Close" : readOnly ? "View" : "Edit"} ${rule.name}`
            }
            onClick={onToggle}
          >
            {open ? <ChevronUpIcon /> : <ChevronDownIcon />}
          </button>
        </div>
      </div>
      {otherIssues.map((code) => (
        <p className="field-error" role="alert" key={code}>
          {ruleIssueMessage(code)}
        </p>
      ))}
      {open ? (
        <div className="rule-body" id={bodyId}>
          {rule.builtin ? (
            <p className="muted">
              This rule matches when what the person recorded for the message&apos;s channel and topic does not allow the send: they
              unsubscribed, or the topic needs a subscription and they have none. A topic&apos;s setting decides what no answer means. Move
              the rule to choose which other rules are checked before it.
            </p>
          ) : (
            <>
              <TextField
                label="Rule name"
                hint="Shown in the message log when this rule stops a send."
                value={rule.name}
                onChange={(event) => onRename(event.target.value)}
                maxLength={MAX_RULE_NAME}
                autoComplete="off"
                disabled={readOnly}
                error={nameError}
              />
              <fieldset className="rule-condition">
                <legend className="field-label">Skip the send when</legend>
                {rule.condition === null ? null : (
                  <ConditionEditor
                    workspaceId={workspaceId}
                    value={rule.condition}
                    onChange={onCondition}
                    readOnly={readOnly}
                    mode={POLICY_MODE}
                    dimensions={dimensions}
                    issues={conditionIssues}
                  />
                )}
              </fieldset>
              <div className="rule-foot">
                {readOnly ? null : (
                  <Button onClick={onDelete} aria-label={`Delete ${rule.name}`}>
                    Delete rule…
                  </Button>
                )}
                <span className="faint">
                  Outcome: skip the send, recorded as <span className="mono">no_consent</span> with this rule&apos;s name.
                </span>
              </div>
            </>
          )}
        </div>
      ) : null}
    </li>
  );
}
