import { useEffect, useRef, type ReactNode } from "react";

import { Button, defaultEventFilters, SelectField, TextField } from "../../ui";
import type { Catalog } from "../segments";
import { DurationField, EventFilters, EventSelect, fieldError, SegmentSelect } from "./fields";
import { issueText, type Section } from "./issues";
import { NIL_UUID, type FlowIssue, type Settings } from "./model";
import { blankTrigger, TRIGGER_KINDS, type TriggerType } from "./triggers";
import { WarmupSetting, type Warmup } from "./Warmup";

type Trigger = Settings["trigger"];
type Goal = NonNullable<Settings["goal"]>;
type Criterion = NonNullable<Settings["exit_criteria"]>[number];

const MAX_EXIT_CRITERIA = 10;

export interface SettingsFocus {
  section: Section;
  /** Changes each time the same section is asked for again. */
  nonce: number;
}

interface Props {
  workspaceId: string;
  settings: Settings;
  catalog: Catalog | null;
  readOnly: boolean;
  issues: ReadonlyMap<Section, readonly FlowIssue[]>;
  focus: SettingsFocus | null;
  onChange: (change: (settings: Settings) => Settings, key?: string) => void;
  /** The warm-up of the flow: its own setting, saved apart from the draft; not shown for a past version. */
  warmup?: { value: Warmup; canChange: boolean; busy: boolean; onSave: (enabled: boolean, seconds: number) => void };
}

function SectionBlock({
  id,
  label,
  issues,
  focus,
  children,
}: {
  id: Section;
  label: string;
  issues: readonly FlowIssue[];
  focus: SettingsFocus | null;
  children: ReactNode;
}) {
  const heading = useRef<HTMLHeadingElement>(null);
  const nonce = focus?.section === id ? focus.nonce : null;
  useEffect(() => {
    if (nonce === null) return;
    heading.current?.scrollIntoView({ block: "start" });
    heading.current?.focus();
  }, [nonce]);
  const headingId = `flow-section-${id}`;
  return (
    <section className="panel panel-pad" aria-labelledby={headingId} id={`${headingId}-section`}>
      <h2 className="panel-title" id={headingId} ref={heading} tabIndex={-1}>
        {label}
      </h2>
      <div className="stack">
        {issues.length > 0 ? (
          <div className="form-error" role="group" aria-label={`Problems with ${label.toLowerCase()}`}>
            <ul className="list-plain">
              {issues.map((issue, index) => (
                <li key={`${issue.code}:${issue.at ?? ""}:${index}`}>{issueText(issue)}</li>
              ))}
            </ul>
          </div>
        ) : null}
        {children}
      </div>
    </section>
  );
}

/** The flow settings: what starts it, who may enter again, the goal and what ends a person's run. */
export function SettingsView({
  workspaceId,
  settings,
  catalog,
  readOnly,
  issues,
  focus,
  onChange,
  warmup,
}: Props) {
  const empty: readonly FlowIssue[] = [];
  const at = (section: Section) => issues.get(section) ?? empty;
  const trigger = settings.trigger;
  const criteria = settings.exit_criteria ?? [];

  const setTrigger = (next: Trigger, key?: string) => onChange((current) => ({ ...current, trigger: next }), key);
  const setGoal = (goal: Goal | undefined, key?: string) =>
    onChange(
      (current) => (goal === undefined ? { ...current, goal: undefined, exit_on_goal: undefined } : { ...current, goal }),
      key,
    );
  const setCriteria = (next: Criterion[], key?: string) =>
    onChange((current) => ({ ...current, exit_criteria: next.length === 0 ? undefined : next }), key);

  return (
    <div className="fl-settings">
      {readOnly ? <p className="muted">You can look at these settings but not change them.</p> : null}
      <SectionBlock id="trigger" label="Trigger" issues={at("trigger")} focus={focus}>
        <SelectField
          label="Starts when"
          value={trigger.type}
          onChange={(event) => {
            const type = event.target.value as TriggerType;
            if (type !== trigger.type) setTrigger(blankTrigger(type));
          }}
          disabled={readOnly}
        >
          {TRIGGER_KINDS.map((kind) => (
            <option key={kind.type} value={kind.type}>
              {kind.label}
            </option>
          ))}
        </SelectField>
        {trigger.type === "segment" ? (
          <>
            <SegmentSelect
              label="Segment"
              value={trigger.segment_id}
              onChange={(segment_id) => setTrigger({ ...trigger, segment_id })}
              catalog={catalog}
              disabled={readOnly}
              error={fieldError(at("trigger"), "/trigger/segment_id")}
            />
            <SelectField label="When a person" value={trigger.on} onChange={(event) => setTrigger({ ...trigger, on: event.target.value === "exited" ? "exited" : "entered" })} disabled={readOnly}>
              <option value="entered">Enters the segment</option>
              <option value="exited">Leaves the segment</option>
            </SelectField>
            <label className="check">
              <input
                type="checkbox"
                checked={trigger.enroll_current === true}
                disabled={readOnly}
                onChange={(event) => setTrigger({ ...trigger, enroll_current: event.target.checked })}
              />
              Also enroll the people already in the segment when the flow is published
            </label>
          </>
        ) : null}
        {trigger.type === "event" ? (
          <>
            <EventSelect
              label="Event"
              value={trigger.event}
              onChange={(event) => setTrigger({ ...trigger, event, filters: defaultEventFilters(event) })}
              catalog={catalog}
              disabled={readOnly}
              error={fieldError(at("trigger"), "/trigger/event")}
            />
            <EventFilters
              workspaceId={workspaceId}
              event={trigger.event}
              filters={trigger.filters ?? []}
              onChange={(filters) => setTrigger({ ...trigger, filters }, "trigger:filters")}
              disabled={readOnly}
              at="/trigger/filters"
              issues={at("trigger")}
            />
          </>
        ) : null}
        {trigger.type === "schedule" ? (
          <>
            <TextField
              label="Cron expression"
              mono
              value={trigger.cron}
              onChange={(event) => setTrigger({ ...trigger, cron: event.target.value }, "trigger:cron")}
              disabled={readOnly}
              autoComplete="off"
              spellCheck={false}
              hint="Five fields: minute, hour, day of month, month, day of week. 0 9 * * 1 is every Monday at 09:00."
              error={fieldError(at("trigger"), "/trigger/cron")}
            />
            <TextField
              label="Time zone"
              value={trigger.timezone}
              onChange={(event) => setTrigger({ ...trigger, timezone: event.target.value }, "trigger:timezone")}
              disabled={readOnly}
              autoComplete="off"
              spellCheck={false}
              hint="A name such as Europe/Copenhagen."
              error={fieldError(at("trigger"), "/trigger/timezone")}
            />
            <SelectField
              label="Enrolls"
              value={trigger.audience.type}
              onChange={(event) =>
                setTrigger({ ...trigger, audience: event.target.value === "segment" ? { type: "segment", segment_id: NIL_UUID } : { type: "everyone" } })
              }
              disabled={readOnly}
            >
              <option value="everyone">Everyone</option>
              <option value="segment">The members of a segment</option>
            </SelectField>
            {trigger.audience.type === "segment" ? (
              <SegmentSelect
                label="Audience segment"
                value={trigger.audience.segment_id}
                onChange={(segment_id) => setTrigger({ ...trigger, audience: { type: "segment", segment_id } })}
                catalog={catalog}
                disabled={readOnly}
                error={fieldError(at("trigger"), "/trigger/audience")}
              />
            ) : null}
          </>
        ) : null}
        {trigger.type === "manual" ? <p className="muted">People are enrolled by hand, one by one or in bulk, by someone with access.</p> : null}
        {trigger.type === "api" ? <p className="muted">People are enrolled by a call to the enrollments endpoint of this flow.</p> : null}
      </SectionBlock>

      <SectionBlock id="reentry" label="Re-entry" issues={at("reentry")} focus={focus}>
        <SelectField
          label="A person who has been in the flow"
          value={settings.reentry.mode}
          onChange={(event) => {
            const mode = event.target.value;
            onChange((current) => ({
              ...current,
              reentry: mode === "always" ? { mode: "always", min_interval_seconds: 0 } : mode === "once_at_a_time" ? { mode: "once_at_a_time" } : { mode: "once" },
            }));
          }}
          disabled={readOnly}
        >
          <option value="once">Never enters again</option>
          <option value="once_at_a_time">Enters again once the earlier run has ended</option>
          <option value="always">Enters again, after a pause</option>
        </SelectField>
        {settings.reentry.mode === "always" ? (
          <DurationField
            legend="At least this long since the last entry"
            seconds={settings.reentry.min_interval_seconds ?? 0}
            onChange={(seconds) => onChange((current) => ({ ...current, reentry: { mode: "always", min_interval_seconds: seconds } }), "reentry:interval")}
            disabled={readOnly}
            error={fieldError(at("reentry"), "/reentry")}
          />
        ) : null}
      </SectionBlock>

      <SectionBlock id="goal" label="Goal" issues={at("goal")} focus={focus}>
        <SelectField
          label="The flow has succeeded when"
          value={settings.goal?.type ?? ""}
          onChange={(event) => {
            const type = event.target.value;
            setGoal(type === "event" ? { type: "event", event: "" } : type === "segment" ? { type: "segment", segment_id: NIL_UUID } : undefined);
          }}
          disabled={readOnly}
        >
          <option value="">There is no goal</option>
          <option value="event">An event happens</option>
          <option value="segment">The person enters a segment</option>
        </SelectField>
        {settings.goal?.type === "event" ? (
          <>
            <EventSelect
              label="Goal event"
              value={settings.goal.event}
              onChange={(event) => setGoal({ type: "event", event, filters: defaultEventFilters(event) })}
              catalog={catalog}
              disabled={readOnly}
              error={fieldError(at("goal"), "/goal/event")}
            />
            <EventFilters
              workspaceId={workspaceId}
              event={settings.goal.event}
              filters={settings.goal.filters ?? []}
              onChange={(filters) => setGoal({ type: "event", event: (settings.goal as { event: string }).event, filters }, "goal:filters")}
              disabled={readOnly}
              at="/goal/filters"
              issues={at("goal")}
            />
          </>
        ) : null}
        {settings.goal?.type === "segment" ? (
          <SegmentSelect
            label="Goal segment"
            value={settings.goal.segment_id}
            onChange={(segment_id) => setGoal({ type: "segment", segment_id })}
            catalog={catalog}
            disabled={readOnly}
            error={fieldError(at("goal"), "/goal/segment_id")}
          />
        ) : null}
        {settings.goal ? (
          <label className="check">
            <input
              type="checkbox"
              checked={settings.exit_on_goal === true}
              disabled={readOnly}
              onChange={(event) => onChange((current) => ({ ...current, exit_on_goal: event.target.checked }))}
            />
            End the flow for a person who reaches the goal
          </label>
        ) : null}
      </SectionBlock>

      <SectionBlock id="exit" label="Exit criteria" issues={at("exit")} focus={focus}>
        <p className="muted">A person still in the flow leaves it when any of these happens.</p>
        {criteria.length === 0 ? <p className="faint">None. A person stays until they reach an exit step.</p> : null}
        {criteria.map((criterion, index) => (
          <fieldset key={index} className="fl-fieldset" aria-label={`Exit rule ${index + 1}`}>
            <legend className="field-label">Exit rule {index + 1}</legend>
            <SelectField
              label="Leaves when"
              value={criterion.type}
              onChange={(event) => {
                const type = event.target.value;
                const next: Criterion = type === "event" ? { type: "event", event: "" } : type === "goal_reached" ? { type: "goal_reached" } : { type: "leaves_segment" };
                setCriteria(criteria.map((entry, at) => (at === index ? next : entry)));
              }}
              disabled={readOnly}
            >
              <option value="leaves_segment">They leave the trigger segment</option>
              <option value="event">An event happens</option>
              <option value="goal_reached">They reach the goal</option>
            </SelectField>
            {criterion.type === "event" ? (
              <>
                <EventSelect
                  label="Event"
                  value={criterion.event}
                  onChange={(event) => setCriteria(criteria.map((entry, at) => (at === index ? { type: "event", event, filters: defaultEventFilters(event) } : entry)))}
                  catalog={catalog}
                  disabled={readOnly}
                  error={fieldError(at("exit"), `/exit_criteria/${index}/event`)}
                />
                <EventFilters
                  workspaceId={workspaceId}
                  event={criterion.event}
                  filters={criterion.filters ?? []}
                  onChange={(filters) => setCriteria(criteria.map((entry, at) => (at === index ? { type: "event", event: criterion.event, filters } : entry)), `exit:${index}:filters`)}
                  disabled={readOnly}
                  at={`/exit_criteria/${index}/filters`}
                  issues={at("exit")}
                />
              </>
            ) : null}
            {readOnly ? null : (
              <Button aria-label={`Remove exit rule ${index + 1}`} onClick={() => setCriteria(criteria.filter((_, at) => at !== index))}>
                Remove this rule
              </Button>
            )}
          </fieldset>
        ))}
        {readOnly || criteria.length >= MAX_EXIT_CRITERIA ? null : (
          <Button onClick={() => setCriteria([...criteria, { type: "leaves_segment" }])}>Add an exit rule</Button>
        )}
      </SectionBlock>

      {warmup ? (
        <section className="panel panel-pad" aria-labelledby="flow-section-warmup">
          <h2 className="panel-title" id="flow-section-warmup">
            Warm-up
          </h2>
          <WarmupSetting warmup={warmup.value} canChange={warmup.canChange} busy={warmup.busy} onSave={warmup.onSave} />
        </section>
      ) : null}
    </div>
  );
}

