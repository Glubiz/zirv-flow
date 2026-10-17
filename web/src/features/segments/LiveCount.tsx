import { Link } from "react-router";

import { type components } from "../../api";
import { Button, EmptyState, ErrorState, formatDateTime, HiddenValue, LoadingState, Panel, shortId } from "../../ui";
import { plural } from "./labels";
import type { LivePreview, PreviewResult } from "./useLivePreview";

type DataAge = components["schemas"]["DataAgeBody"];
type Display = components["schemas"]["DisplayBody"];

export function DataAgeNote({ age }: { age: DataAge }) {
  const headline = age.live
    ? "Live: this reads only data that is always current."
    : age.as_of
      ? `Data as of ${formatDateTime(age.as_of)}.`
      : "The age of the data is unknown: a source that feeds this has never succeeded.";
  return (
    <div className="stack-tight">
      <p className="muted">{headline}</p>
      {age.sources.length > 0 ? (
        <ul className="list-plain stack-tight" aria-label="Sources that feed this">
          {age.sources.map((source) => (
            <li className="faint" key={source.source_id}>
              {source.name}: {source.last_success_at ? `last succeeded ${formatDateTime(source.last_success_at)}` : "has never succeeded"}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

export function personLabel(id: string, display: Display | null | undefined): React.ReactNode {
  if (display?.masked) return <HiddenValue what="This person's identifier" />;
  return display?.value ? display.value : `Person ${shortId(id)}`;
}

function Result({ result, updating, workspaceId }: { result: PreviewResult; updating: boolean; workspaceId: string }) {
  return (
    <div className="stack">
      <div>
        <p className="live-count">
          <span className="live-count-number">{result.count}</span> {result.count === 1 ? "person matches" : "people match"}
        </p>
        {updating ? <p className="faint">Updating the count</p> : null}
      </div>
      {result.count === 0 ? <EmptyState title="No one matches yet">Nobody in the workspace fits this definition right now.</EmptyState> : null}
      <DataAgeNote age={result.data_age} />
      {result.sample.length > 0 ? (
        <section aria-label="Sample of people">
          <h3 className="strong">A sample</h3>
          <ul className="list-plain stack-tight">
            {result.sample.map((person) => (
              <li key={person.id}>
                <Link to={`/w/${workspaceId}/people/${person.id}`}>{personLabel(person.id, person.display)}</Link>
              </li>
            ))}
          </ul>
          {result.count > result.sample.length ? (
            <p className="faint">Showing {plural(result.sample.length, "person", "people")} of {result.count}.</p>
          ) : null}
        </section>
      ) : null}
    </div>
  );
}

/** The live count beside the builder: idle, counting, ready, empty, timed out, refused or failed. */
export function LiveCount({ workspaceId, preview }: { workspaceId: string; preview: LivePreview }) {
  const { state, retry } = preview;
  let body: React.ReactNode;
  switch (state.status) {
    case "incomplete":
      body = <p className="muted">Finish the condition to see a count.</p>;
      break;
    case "loading":
      body = state.previous ? (
        <Result result={state.previous} updating workspaceId={workspaceId} />
      ) : (
        <LoadingState label="Counting people" />
      );
      break;
    case "ready":
      body = <Result result={state.result} updating={false} workspaceId={workspaceId} />;
      break;
    case "timeout":
      body = (
        <div className="state state-error" role="alert">
          <p className="state-title">The count took too long</p>
          <p className="state-body">Counting this many people ran past its time limit. Narrow the definition, or try again.</p>
          <div className="state-actions">
            <Button onClick={retry}>Try again</Button>
          </div>
        </div>
      );
      break;
    case "issues":
      body = (
        <p className="muted" role="status">
          The definition has problems, marked on the conditions. Fix them to see a count.
        </p>
      );
      break;
    case "error":
      body = <ErrorState title={state.error.title} detail={state.error.detail} traceId={state.error.traceId} onRetry={retry} />;
      break;
  }
  return (
    <Panel title="Who matches">
      <div aria-live="polite">{body}</div>
    </Panel>
  );
}
