import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, useResource, type components, type ErrorSummary } from "../../api";
import { Button, InlineError, SelectField, TextField } from "../../ui";
import type { Dimension } from "./DimensionDialog";
import { BlockedIcon, CheckIcon } from "./policyIcons";

export type Evaluation = components["schemas"]["ConsentPolicyEvaluationBody"];

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

interface EvaluatePanelProps {
  workspaceId: string;
  dimensions: readonly Dimension[];
  /** Told after each check, so the rule list can mark the rule that decided. */
  onResult: (result: Evaluation | null) => void;
}

/** Runs the published policy for one person and one message and names the rule that decides. Nothing is sent. */
export function EvaluatePanel({ workspaceId, dimensions, onResult }: EvaluatePanelProps) {
  const topics = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/topics", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [person, setPerson] = useState("");
  const [topic, setTopic] = useState("");
  const [scope, setScope] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [result, setResult] = useState<Evaluation | null>(null);

  const declared = dimensions.filter((dimension) => !dimension.builtin && !dimension.archived);
  const choices = topics.status === "ready" ? topics.data.filter((candidate) => !candidate.archived) : [];

  async function resolve(text: string): Promise<string | null> {
    const trimmed = text.trim();
    if (UUID.test(trimmed)) return trimmed;
    const found = await unwrap(
      client.POST("/api/v1/workspaces/{ws}/people/search", {
        params: { path: { ws: workspaceId } },
        body: { identifier: { kind: "email", value: trimmed }, limit: 1 },
      }),
    );
    return found.people[0]?.id ?? null;
  }

  async function run(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      const id = await resolve(person);
      if (id === null) {
        setResult(null);
        onResult(null);
        setFailure({ title: "No such person", detail: "Check the email address or paste the person id from their profile address.", traceId: null });
        return;
      }
      const dimensionValues = Object.fromEntries(Object.entries(scope).filter(([key, value]) => value !== "" && declared.some((entry) => entry.key === key)));
      const evaluation = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/consent/policy/evaluate", {
          params: { path: { ws: workspaceId } },
          body: {
            person_id: id,
            channel: "email",
            ...(topic === "" ? {} : { topic_key: topic }),
            ...(Object.keys(dimensionValues).length > 0 ? { dimensions: dimensionValues } : {}),
          },
        }),
      );
      setResult(evaluation);
      onResult(evaluation);
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <aside aria-labelledby="check-h" className="panel panel-pad policy-aside">
      <div>
        <h2 id="check-h" className="panel-title">
          Check a person
        </h2>
        <p className="muted">Runs the published policy for one person and one message. Nothing is sent.</p>
      </div>
      <form className="stack" onSubmit={(event) => void run(event)} aria-label="Check a person" noValidate>
        <TextField
          label="Person"
          hint="An email address, or the person id from their profile address."
          value={person}
          onChange={(event) => setPerson(event.target.value)}
          autoComplete="off"
        />
        <SelectField label="Topic" value={topic} onChange={(event) => setTopic(event.target.value)}>
          <option value="">No topic</option>
          {choices.map((candidate) => (
            <option key={candidate.id} value={candidate.key}>
              {candidate.name}
            </option>
          ))}
        </SelectField>
        {declared.map((dimension) => (
          <SelectField
            key={dimension.key}
            label={dimension.name}
            value={scope[dimension.key] ?? ""}
            onChange={(event) => setScope((current) => ({ ...current, [dimension.key]: event.target.value }))}
          >
            <option value="">Not set</option>
            {dimension.values
              .filter((value) => !value.archived)
              .map((value) => (
                <option key={value.key} value={value.key}>
                  {value.name}
                </option>
              ))}
          </SelectField>
        ))}
        <div>
          <Button type="submit" disabled={busy || person.trim() === ""}>
            {busy ? "Checking" : "Check"}
          </Button>
        </div>
      </form>
      {failure ? <InlineError {...failure} /> : null}
      {result ? <EvaluationResult result={result} /> : null}
    </aside>
  );
}

function EvaluationResult({ result }: { result: Evaluation }) {
  const version = result.policy_version;
  return (
    <div role="status" className="policy-result">
      {result.allowed ? (
        <div className="st st-ok policy-verdict">
          <CheckIcon />
          No rule matched: send
        </div>
      ) : (
        <div className="st st-skip policy-verdict">
          <BlockedIcon />
          Would skip: <span className="mono">no_consent</span>
        </div>
      )}
      {result.rule ? (
        <p>
          Decided by <strong>{result.rule.name}</strong>.
          {result.rule.unevaluable ? " It stops the send because something it reads was archived since it was published." : ""}
        </p>
      ) : (
        <p>The send goes on to the suppression check.</p>
      )}
      <p className="faint">
        {version === null || version === undefined
          ? "Nothing is published yet, so only the built-in recorded consent rule applied."
          : `Checked against published version ${version}, not the draft.`}
      </p>
    </div>
  );
}
