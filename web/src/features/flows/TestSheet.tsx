import { useEffect, useRef, useState, type SyntheticEvent } from "react";

import { ApiError, client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, EmptyState, formatDateTime, InlineError, Pill, SelectField, Sheet, TextAreaField, TextField } from "../../ui";
import { issueText } from "./issues";
import type { FlowIssue, Graph } from "./model";
import {
  advanceBy,
  isObject,
  parseObject,
  pretty,
  UNITS,
  utcInstant,
  walkRequest,
  type Effect,
  type PathStep,
  type ScriptStep,
  type Snapshot,
  type TestResult,
  type UnitId,
} from "./testMode";

export const NOTHING_SENT = "Nothing is sent or changed: the walk runs on a virtual clock and writes nothing.";

interface PersonChoice {
  id: string;
  label: string;
}

interface Props {
  workspaceId: string;
  flowId: string;
  /** A real person's values need the right to see them, which the personal data permission grants. */
  canSeePeople: boolean;
  /** A step in words, such as "Branch: Opened onboarding?". */
  describeStep: (id: string) => string;
  /** The name of an output of a step, such as "Yes". */
  portName: (id: string, port: string) => string | null;
  /** The draft as it is on the canvas, saved or not: every walk carries it, so a test writes nothing. */
  graph: Graph;
  /** The draft's settings are not saved. The walk takes the goal and exit settings from the saved draft, as the request has no field for them. */
  settingsUnsaved: boolean;
  onResult: (result: TestResult | null) => void;
  onClose: () => void;
  announce: (text: string) => void;
}

/** The right-hand sheet of test mode: who is walked, the script, the path and what the walk captured. */
export function TestSheet(props: Props) {
  const { workspaceId, flowId, describeStep, portName } = props;
  const [who, setWho] = useState<"person" | "snapshot">(props.canSeePeople ? "person" : "snapshot");
  const [person, setPerson] = useState<PersonChoice | null>(null);
  const [snapshotText, setSnapshotText] = useState("{}");
  const [startText, setStartText] = useState("");
  const [script, setScript] = useState<ScriptStep[]>([]);
  const [result, setResult] = useState<TestResult | null>(null);
  const [form, setForm] = useState<"advance" | "event" | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  /** The issues that stopped the walk: the draft is not valid. */
  const [invalid, setInvalid] = useState<ApiError | null>(null);
  const inFlight = useRef(false);
  /** Bumped when the run starts over or the sheet goes, so a late answer is dropped. */
  const run = useRef(0);

  useEffect(() => {
    return () => {
      run.current += 1;
    };
  }, []);

  /** Walks the scenario with this script; true when the answer was taken. */
  async function walk(next: ScriptStep[]): Promise<boolean> {
    if (inFlight.current) return false;
    let target: { personId: string } | { snapshot: Snapshot };
    let startAt: string | null = null;
    if (result === null) {
      if (who === "person") {
        if (person === null) {
          setProblem("Search for a person and choose one, or walk a made-up person.");
          return false;
        }
        target = { personId: person.id };
      } else {
        const parsed = parseObject(snapshotText);
        if ("problem" in parsed) {
          setProblem(`The snapshot: ${parsed.problem}`);
          return false;
        }
        target = { snapshot: parsed.value as unknown as Snapshot };
      }
      if (startText.trim() !== "") {
        startAt = utcInstant(startText);
        if (startAt === null) {
          setProblem("The start time is not a time.");
          return false;
        }
      }
    } else {
      target = { snapshot: result.snapshot };
    }
    setProblem(null);
    setFailure(null);
    setInvalid(null);
    const ticket = run.current;
    inFlight.current = true;
    setBusy(true);
    try {
      const body = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/flows/{id}/test", {
          params: { path: { ws: workspaceId, id: flowId } },
          body: walkRequest(target, startAt, next, result, props.graph),
        }),
      );
      if (ticket !== run.current) return false;
      setResult(body);
      setScript(next);
      props.onResult(body);
      props.announce(
        body.halted
          ? `Unable to walk further: ${body.halted.message}`
          : `The test walked ${body.path.length} ${body.path.length === 1 ? "step" : "steps"}. Nothing was sent or changed.`,
      );
      return true;
    } catch (error) {
      if (ticket === run.current) {
        if (error instanceof ApiError && error.code === "flow_invalid") setInvalid(error);
        else setFailure(describeError(error));
      }
      return false;
    } finally {
      if (ticket === run.current) {
        inFlight.current = false;
        setBusy(false);
      }
    }
  }

  /** Why the draft cannot be walked, in words and next to the button that was pressed. */
  const invalidNote = invalid ? (
    <div className="form-error" role="alert">
      <span className="strong">The draft has issues that stop a test. Fix them, then start again.</span>
      {invalid.flowIssues.length > 0 ? (
        <ul className="list-plain" aria-label="Issues that stop the test">
          {invalid.flowIssues.map((issue, index) => (
            <li key={`${index}:${issue.code}`}>
              {issue.node ? `${props.describeStep(issue.node)}: ` : ""}
              {issueText(issue as FlowIssue, props.graph)}
            </li>
          ))}
        </ul>
      ) : (
        <span>The issues are marked on the canvas and listed in the editor.</span>
      )}
    </div>
  ) : null;

  function startOver() {
    run.current += 1;
    inFlight.current = false;
    setResult(null);
    setScript([]);
    setForm(null);
    setFailure(null);
    setInvalid(null);
    setProblem(null);
    setBusy(false);
    props.onResult(null);
    props.announce("The test run started over.");
  }

  return (
    <Sheet title={result ? "Test run" : "Set up a test run"} eyebrow="Test mode · the draft" closeLabel="Close" onClose={props.onClose}>
      <p className="muted">{NOTHING_SENT}</p>
      {props.settingsUnsaved ? (
        <p className="muted">Your settings changes aren&apos;t saved yet. This test uses the saved trigger, goal and exit settings.</p>
      ) : null}
      <div className="fl-sheet-section">
        {result === null ? (
          <>
            <fieldset className="fl-fieldset">
              <legend className="strong">Who walks the flow</legend>
              <label className="fl-radio">
                <input
                  type="radio"
                  name="test-who"
                  checked={who === "person"}
                  disabled={!props.canSeePeople}
                  onChange={() => setWho("person")}
                />
                <span>A real person</span>
              </label>
              {!props.canSeePeople ? (
                <p className="faint">Walking a real person needs the personal data permission.</p>
              ) : null}
              <label className="fl-radio">
                <input type="radio" name="test-who" checked={who === "snapshot"} onChange={() => setWho("snapshot")} />
                <span>A made-up person, from an editable snapshot</span>
              </label>
            </fieldset>
            {who === "person" ? (
              <PersonPicker workspaceId={workspaceId} chosen={person} onChoose={setPerson} />
            ) : (
              <TextAreaField
                label="Snapshot"
                mono
                rows={7}
                spellCheck={false}
                hint='JSON with "traits", "lists", "segments", "events" and "capped_events". Leave {} for a person with nothing yet.'
                value={snapshotText}
                onChange={(event) => setSnapshotText(event.target.value)}
              />
            )}
            <TextField
              label="Start time (UTC)"
              type="datetime-local"
              hint="Leave empty to start now."
              value={startText}
              onChange={(event) => setStartText(event.target.value)}
            />
            <div>
              <Button variant="primary" aria-disabled={busy || undefined} onClick={() => void walk([])}>
                {busy ? "Walking…" : "Start the test"}
              </Button>
            </div>
            {invalidNote}
          </>
        ) : (
          <>
            <div className="row">
              <span className="strong">
                {result.person_id ? "A real person" : "A made-up person"} · from {formatDateTime(result.start_at)}
              </span>
              <Button aria-disabled={busy || undefined} onClick={startOver}>
                Start over
              </Button>
            </div>
            <div className="row" role="group" aria-label="Script">
              <Button
                variant="primary"
                aria-disabled={busy || undefined}
                aria-expanded={form === "advance"}
                onClick={() => setForm(form === "advance" ? null : "advance")}
              >
                Advance the clock…
              </Button>
              <Button aria-disabled={busy || undefined} aria-expanded={form === "event"} onClick={() => setForm(form === "event" ? null : "event")}>
                Send an event…
              </Button>
            </div>
            {form === "advance" ? (
              <AdvanceForm
                busy={busy}
                clock={result.final_state.clock}
                onSubmit={async (to) => {
                  if (await walk([...script, { advance_to: to }])) setForm(null);
                }}
              />
            ) : null}
            {form === "event" ? (
              <EventForm
                busy={busy}
                clock={result.final_state.clock}
                onSubmit={async (event) => {
                  if (await walk([...script, { inject: event }])) setForm(null);
                }}
              />
            ) : null}
            {invalidNote}
          </>
        )}
        {problem ? <InlineError title={problem} /> : null}
        {failure ? <InlineError {...failure} /> : null}
        {busy ? (
          <p className="muted" role="status">
            Walking the flow
          </p>
        ) : null}
      </div>
      {result ? <Outcome result={result} script={script} describeStep={describeStep} portName={portName} /> : null}
    </Sheet>
  );
}

function PersonPicker({
  workspaceId,
  chosen,
  onChoose,
}: {
  workspaceId: string;
  chosen: PersonChoice | null;
  onChoose: (person: PersonChoice) => void;
}) {
  const [kind, setKind] = useState("email");
  const [value, setValue] = useState("");
  const [searching, setSearching] = useState(false);
  const [found, setFound] = useState<PersonChoice[] | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function search(event: SyntheticEvent) {
    event.preventDefault();
    if (searching || value.trim() === "") return;
    setSearching(true);
    setFailure(null);
    try {
      const body = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/people/search", {
          params: { path: { ws: workspaceId } },
          body: { identifier: { kind, value: value.trim() }, limit: 10 },
        }),
      );
      setFound(
        body.people.map((hit) => ({
          id: hit.id,
          label: hit.display?.value ? hit.display.value : `Person ${hit.id.slice(0, 8)}`,
        })),
      );
    } catch (error) {
      setFound(null);
      setFailure(describeError(error));
    } finally {
      setSearching(false);
    }
  }

  return (
    <form className="stack" aria-label="Find a person" onSubmit={(event) => void search(event)}>
      <div className="row">
        <SelectField label="Search by" value={kind} onChange={(event) => setKind(event.target.value)}>
          <option value="email">Email</option>
          <option value="phone">Phone</option>
          <option value="anonymous_id">Anonymous ID</option>
        </SelectField>
        <TextField label="Search people" value={value} autoComplete="off" onChange={(event) => setValue(event.target.value)} />
        <Button type="submit" aria-disabled={searching || undefined}>
          {searching ? "Searching…" : "Search"}
        </Button>
      </div>
      {failure ? <InlineError {...failure} /> : null}
      {found !== null && found.length === 0 ? <p className="muted">No one matches that.</p> : null}
      {found !== null && found.length > 0 ? (
        <ul className="list-plain" aria-label="People found">
          {found.map((hit) => (
            <li key={hit.id} className="row">
              <span>{hit.label}</span>
              <Button aria-label={`Use ${hit.label}`} aria-pressed={chosen?.id === hit.id} onClick={() => onChoose(hit)}>
                {chosen?.id === hit.id ? "Chosen" : "Use"}
              </Button>
            </li>
          ))}
        </ul>
      ) : null}
      {chosen ? (
        <p>
          Walking <span className="strong">{chosen.label}</span>
        </p>
      ) : null}
    </form>
  );
}

function AdvanceForm({ busy, clock, onSubmit }: { busy: boolean; clock: string; onSubmit: (to: string) => Promise<void> }) {
  const [amount, setAmount] = useState("1");
  const [unit, setUnit] = useState<UnitId>("days");
  const [problem, setProblem] = useState<string | null>(null);
  function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (busy) return;
    const count = Number(amount);
    if (!Number.isFinite(count) || count <= 0) {
      setProblem("Enter an amount above zero.");
      return;
    }
    setProblem(null);
    void onSubmit(advanceBy(clock, count, unit));
  }
  return (
    <form className="fl-effect" aria-label="Advance the clock" onSubmit={submit}>
      <p className="faint">The clock is at {formatDateTime(clock)}. Every timer due by the new time fires.</p>
      <div className="row">
        <TextField label="Advance by" inputMode="decimal" value={amount} onChange={(event) => setAmount(event.target.value)} />
        <SelectField label="Unit" value={unit} onChange={(event) => setUnit(event.target.value as UnitId)}>
          {UNITS.map((entry) => (
            <option key={entry.id} value={entry.id}>
              {entry.label}
            </option>
          ))}
        </SelectField>
      </div>
      {problem ? <InlineError title={problem} /> : null}
      <div>
        <Button type="submit" variant="primary" aria-disabled={busy || undefined}>
          Advance
        </Button>
      </div>
    </form>
  );
}

function EventForm({
  busy,
  clock,
  onSubmit,
}: {
  busy: boolean;
  clock: string;
  onSubmit: (event: { name: string; properties: Record<string, never>; at: string }) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [properties, setProperties] = useState("{}");
  const [problem, setProblem] = useState<string | null>(null);
  function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (busy) return;
    if (name.trim() === "") {
      setProblem("Enter the name of the event.");
      return;
    }
    const parsed = parseObject(properties);
    if ("problem" in parsed) {
      setProblem(`The properties: ${parsed.problem}`);
      return;
    }
    setProblem(null);
    void onSubmit({ name: name.trim(), properties: parsed.value as Record<string, never>, at: clock });
  }
  return (
    <form className="fl-effect" aria-label="Send an event" onSubmit={submit}>
      <p className="faint">The event arrives at {formatDateTime(clock)}, where the clock is now.</p>
      <TextField label="Event name" mono autoComplete="off" value={name} onChange={(event) => setName(event.target.value)} />
      <TextAreaField label="Properties" mono rows={3} spellCheck={false} value={properties} onChange={(event) => setProperties(event.target.value)} />
      {problem ? <InlineError title={problem} /> : null}
      <div>
        <Button type="submit" variant="primary" aria-disabled={busy || undefined}>
          Send event
        </Button>
      </div>
    </form>
  );
}

const OUTCOMES: Record<string, string> = {
  passed: "Passed",
  waiting: "Waiting here",
  completed: "Completed the flow",
  exited: "Left the flow",
  failed: "Failed",
  unable_to_evaluate: "Unable to evaluate",
  converted: "Goal reached",
};

function stepWords(step: PathStep, portName: Props["portName"]): string {
  if (step.outcome === "branched") {
    const taken = step.port ? portName(step.node, step.port) : null;
    return taken ? `Took ${taken}` : "Chose a path";
  }
  if (step.woke_by) {
    const by = step.woke_by === "timer" ? "on time" : step.woke_by === "event" ? "by the event" : "by the timeout";
    return `Woke ${by}`;
  }
  return OUTCOMES[step.outcome] ?? step.outcome;
}

function describeScript(step: ScriptStep): string {
  if (step.advance_to) return `Advanced the clock to ${formatDateTime(step.advance_to)}`;
  if (step.inject) return `Sent ${step.inject.name} at ${formatDateTime(step.inject.at)}`;
  return "A step";
}

const SCRIPT_OUTCOMES: Record<string, string> = {
  advanced: "timers due were fired",
  event_delivered: "it satisfied the wait",
  event_recorded: "no wait was looking for it",
  event_ignored: "its filters did not match",
  goal_reached: "the goal was reached",
  goal_exit: "the goal ended the enrollment",
  exit_criterion: "an exit criterion ended the enrollment",
  skipped: "the walk had already stopped",
};

function Outcome({
  result,
  script,
  describeStep,
  portName,
}: {
  result: TestResult;
  script: readonly ScriptStep[];
  describeStep: Props["describeStep"];
  portName: Props["portName"];
}) {
  const { halted } = result;
  const effects = [
    ...result.external_effects.map((effect) => ({ effect, sent: true })),
    ...result.internal_effects.map((effect) => ({ effect, sent: false })),
  ];
  return (
    <>
      {halted ? (
        <div className="form-error" role="alert">
          <span className="strong">{halted.reason === "unable_to_evaluate" ? "Unable to evaluate" : "The walk stopped"}</span>
          <div>{halted.message}</div>
          {halted.node ? <div>At {describeStep(halted.node)}.</div> : null}
          {halted.capped_events.length > 0 ? (
            <div>
              The snapshot holds too many of {halted.capped_events.join(", ")}
              {halted.cap != null ? ` (it keeps ${halted.cap})` : ""}, so the condition cannot be decided.
            </div>
          ) : null}
        </div>
      ) : null}
      <div className="fl-sheet-section">
        <h3>Steps · now {formatDateTime(result.final_state.clock)}</h3>
        {result.path.length === 0 ? (
          <EmptyState title="The walk did not reach a step">The person did not enter the flow.</EmptyState>
        ) : (
          <ol className="fl-steps" aria-label="Steps of the test path">
            {result.path.map((step, index) => (
              <li key={`${index}:${step.node}`} className="fl-step-row">
                <span className={step.outcome === "failed" || step.outcome === "unable_to_evaluate" ? "fl-step-n fl-step-n-bad" : "fl-step-n"}>
                  {index + 1}
                </span>
                <div className="stack-tight">
                  <div>
                    <span className="strong">{describeStep(step.node)}</span> <span className="faint mono">{formatDateTime(step.at)}</span>
                  </div>
                  <div className="muted">{stepWords(step, portName)}</div>
                  {step.branch?.map((condition) => (
                    <div key={condition.index} className="faint">
                      Condition {condition.index + 1}: {condition.outcome.replace("_", " ")}
                      {condition.observed.map((leaf) => (
                        <span key={leaf.pointer}>
                          {" · observed "}
                          <span className="mono">{JSON.stringify(leaf.observed)}</span> ({leaf.outcome})
                        </span>
                      ))}
                    </div>
                  ))}
                </div>
              </li>
            ))}
          </ol>
        )}
        <p className="muted">
          The person ended up {result.final_state.status.replace("_", " ")}
          {result.final_state.awaiting ? `, waiting for ${result.final_state.awaiting}` : ""}
          {result.final_state.wake_at ? ` until ${formatDateTime(result.final_state.wake_at)}` : ""}.
        </p>
      </div>
      {script.length > 0 ? (
        <div className="fl-sheet-section">
          <h3>Script</h3>
          <ol className="list-plain" aria-label="The script so far">
            {script.map((step, index) => (
              <li key={index}>
                {describeScript(step)}
                {result.script[index] ? (
                  <span className="faint"> · {SCRIPT_OUTCOMES[result.script[index].outcome] ?? result.script[index].outcome}</span>
                ) : null}
              </li>
            ))}
          </ol>
        </div>
      ) : null}
      <div className="fl-sheet-section">
        <h3>Captured effects</h3>
        {effects.length === 0 ? (
          <p className="muted">The walk asked for no webhook, trait change, list change or goal.</p>
        ) : (
          effects.map(({ effect, sent }) => (
            <EffectCard key={`${effect.step_key}:${effect.kind}`} effect={effect} sent={sent} describeStep={describeStep} />
          ))
        )}
        {result.not_simulated.length > 0 ? (
          <p className="faint">A test cannot trigger: {result.not_simulated.map((key) => key.replaceAll("_", " ")).join(", ")}.</p>
        ) : null}
      </div>
    </>
  );
}

const EFFECT_NAMES: Record<string, string> = {
  webhook: "Webhook",
  update_trait: "Update a trait",
  list: "List",
  goal: "Goal",
};

/** The effect's kind and its step, said once when the step's own label already starts with the kind ("Webhook: ..."). */
function effectTitle(kind: string, step: string): string {
  const name = EFFECT_NAMES[kind] ?? kind;
  return step.toLowerCase().startsWith(name.toLowerCase()) ? step : `${name} · ${step}`;
}

function EffectCard({ effect, sent, describeStep }: { effect: Effect; sent: boolean; describeStep: Props["describeStep"] }) {
  const detail = effect.detail as Record<string, unknown>;
  const url = typeof detail["url"] === "string" ? detail["url"] : null;
  const body = effect.kind === "webhook" && isObject(detail["envelope"]) ? detail["envelope"] : detail;
  return (
    <div className="fl-effect">
      <div className="row">
        <span className="strong">
          {effectTitle(effect.kind, describeStep(effect.node))}
        </span>
        <Pill>{sent ? "Not sent" : "Not changed"}</Pill>
      </div>
      {url ? <span className="mono">POST {url}</span> : null}
      <pre className="fl-code" tabIndex={0} aria-label={effect.kind === "webhook" ? "Webhook envelope" : "Effect detail"}>
        {pretty(body)}
      </pre>
    </div>
  );
}
