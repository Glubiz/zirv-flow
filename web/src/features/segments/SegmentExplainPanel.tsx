import { Fragment, useState } from "react";
import { Link } from "react-router";

import { ApiError, client, describeError, unwrap, useResource, type components } from "../../api";
import { Button, EmptyState, ErrorState, formatDateTime, HiddenValue, LoadingState, Panel, SelectField, shortId } from "../../ui";
import { fromDefinition, getAt, place, type Node } from "./ast";
import { describeNode, valueText, type Names } from "./describe";

type Explain = components["schemas"]["ExplainBody"];
type ExplainNode = components["schemas"]["ExplainNodeBody"];
type Observed = components["schemas"]["ObservedBody"];
type Outcome = components["schemas"]["OutcomeBody"];

const OUTCOME: Record<Outcome, { icon: string; word: string }> = {
  pass: { icon: "✓", word: "Passes" },
  fail: { icon: "✗", word: "Fails" },
  unknown: { icon: "?", word: "Unknown" },
};

const KIND_WORDS: Record<string, string> = {
  all: "All of",
  any: "Any of",
  not: "Not",
  trait: "Field",
  event: "Event",
  list: "List",
  segment: "Segment",
  consent: "Consent",
  identifier: "Identifier",
};

function ObservedValue({ observed }: { observed: Observed }) {
  const has = (value: unknown) => value !== undefined && value !== null;
  const parts: React.ReactNode[] = [];
  if (has(observed.count)) {
    parts.push(<>{observed.count} matching {observed.count === 1 ? "event" : "events"}</>);
  }
  if (observed.masked) {
    parts.push(<>{has(observed.count) ? "value " : ""}<HiddenValue what="The value" /></>);
  } else if (observed.missing) {
    parts.push(<>no value</>);
  } else if (has(observed.status)) {
    parts.push(<>consent is {observed.status.replaceAll("_", " ")}</>);
  } else if (has(observed.present)) {
    parts.push(<>{observed.present ? "yes" : "no"}</>);
  } else if (has(observed.value)) {
    parts.push(<>{has(observed.count) ? "value " : ""}{valueText(observed.value)}</>);
  }
  if (parts.length === 0) return null;
  return (
    <>
      Seen:{" "}
      {parts.map((part, at) => (
        <Fragment key={at}>
          {at > 0 ? ", " : ""}
          {part}
        </Fragment>
      ))}
    </>
  );
}

function TreeNode({ node, definition, names }: { node: ExplainNode; definition: Node | null; names: Names }) {
  const outcome = OUTCOME[node.outcome];
  const placement = definition ? place(definition, node.pointer) : null;
  const source = definition && placement ? getAt(definition, placement.path) : undefined;
  const words = source ? describeNode(source, names) : null;
  const unable = node.outcome === "unknown";
  return (
    <li role="treeitem" aria-expanded={node.children.length > 0 ? true : undefined} className="tree-item">
      <div className="tree-row">
        <span className={`tree-outcome tree-${node.outcome}`}>
          <span aria-hidden="true">{outcome.icon}</span> {outcome.word}
        </span>
        <span className="strong">{KIND_WORDS[node.kind] ?? node.kind}</span>
        {words ? <span>{words}</span> : null}
      </div>
      {unable ? (
        <p className="faint">{node.reason === "capped" ? "Unable to explain: too many events to load" : "Unable to explain"}</p>
      ) : null}
      {node.observed ? (
        <p className="faint">
          <ObservedValue observed={node.observed} />
        </p>
      ) : null}
      {node.children.length > 0 ? (
        <ul role="group" className="tree-list">
          {node.children.map((child, index) => (
            <TreeNode key={`${child.pointer}:${index}`} node={child} definition={definition} names={names} />
          ))}
        </ul>
      ) : null}
    </li>
  );
}

function Summary({ explain, workspaceId, currentVersion }: { explain: Explain; workspaceId: string; currentVersion: number | null }) {
  const older = currentVersion !== null && explain.version !== currentVersion;
  const decided = explain.outcome !== "unknown";
  const pending = decided && !older && (explain.outcome === "pass") !== explain.member;
  return (
    <div className="stack-tight">
      <p className="strong">
        {explain.outcome === "pass"
          ? "In the segment"
          : explain.outcome === "fail"
            ? "Not in the segment"
            : "Unable to tell whether this person is in the segment"}
      </p>
      <p className="muted">
        {explain.member
          ? `Stored as a member${explain.entered_at ? ` since ${formatDateTime(explain.entered_at)}` : ""}.`
          : "Stored as not a member."}{" "}
        Worked out at {formatDateTime(explain.evaluated_at)} against version {explain.version}.
      </p>
      {older ? (
        <p className="notice" role="status">
          This is version {explain.version}. The stored membership refers to the current version, {currentVersion}.
        </p>
      ) : null}
      {pending ? (
        <p className="notice" role="status">
          A refresh is pending: the stored membership has not caught up with this answer yet.
        </p>
      ) : null}
      {explain.merged_from ? (
        <p className="notice" role="status">
          The person asked about, {shortId(explain.merged_from)}, was merged into{" "}
          <Link to={`/w/${workspaceId}/people/${explain.person_id}`}>this person</Link>, so the profile of the merged person is explained.
        </p>
      ) : null}
    </div>
  );
}

function Failure({ error, onRetry }: { error: unknown; onRetry: () => void }) {
  if (error instanceof ApiError) {
    if (error.code === "explain_timeout") {
      return (
        <div className="state state-error" role="alert">
          <p className="state-title">The explanation took too long</p>
          <p className="state-body">Working this out ran past its time limit.</p>
          <div className="state-actions">
            <Button onClick={onRetry}>Try again</Button>
          </div>
        </div>
      );
    }
    if (error.status === 404) {
      return <ErrorState title="Not found" detail="The segment, the version or the person was not found." />;
    }
    if (error.status === 422) {
      return (
        <ErrorState
          title="This version no longer validates"
          detail="It refers to something that has since changed. Open the segment and save it again, or choose another version."
        />
      );
    }
  }
  return <ErrorState {...describeError(error)} onRetry={onRetry} />;
}

interface PanelProps {
  workspaceId: string;
  personId: string;
}

/** Why a person is, or is not, in a segment: the condition tree with what was seen at each node. */
export function SegmentExplainPanel({ workspaceId, personId }: PanelProps) {
  const [segmentId, setSegmentId] = useState("");
  const [version, setVersion] = useState("");
  const segments = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/segments", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const lists = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/lists", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const versions = useResource(
    () =>
      segmentId === ""
        ? Promise.resolve([])
        : unwrap(client.GET("/api/v1/workspaces/{ws}/segments/{id}/versions", { params: { path: { ws: workspaceId, id: segmentId } } })),
    [workspaceId, segmentId],
  );
  const explain = useResource(
    () =>
      segmentId === ""
        ? Promise.resolve(null)
        : unwrap(
            client.GET("/api/v1/workspaces/{ws}/people/{id}/explain", {
              params: {
                path: { ws: workspaceId, id: personId },
                query: { segment: segmentId, ...(version === "" ? {} : { version: Number(version) }) },
              },
            }),
          ),
    [workspaceId, personId, segmentId, version],
  );

  const names: Names = {
    lists: new Map(lists.status === "ready" ? lists.data.map((list) => [list.id, list.name]) : []),
    segments: new Map(segments.status === "ready" ? segments.data.map((segment) => [segment.id, segment.name]) : []),
  };

  function definitionOf(result: Explain): Node | null {
    if (versions.status !== "ready") return null;
    const found = versions.data.find((entry) => entry.version === result.version);
    return found ? fromDefinition(found.definition) : null;
  }

  return (
    <Panel title="Segment membership">
      <div className="stack">
        {segments.status === "error" ? (
          <ErrorState {...describeError(segments.error)} onRetry={segments.reload} />
        ) : (
          <div className="inline-form">
            <SelectField
              label="Segment"
              value={segmentId}
              onChange={(event) => {
                setSegmentId(event.target.value);
                setVersion("");
              }}
              disabled={segments.status !== "ready"}
            >
              <option value="">Choose a segment</option>
              {segments.status === "ready"
                ? segments.data.map((segment) => (
                    <option key={segment.id} value={segment.id}>
                      {segment.name}
                    </option>
                  ))
                : null}
            </SelectField>
            <SelectField
              label="Version"
              value={version}
              onChange={(event) => setVersion(event.target.value)}
              disabled={segmentId === "" || versions.status !== "ready"}
            >
              <option value="">The current version</option>
              {versions.status === "ready"
                ? versions.data.map((entry) => (
                    <option key={entry.version} value={String(entry.version)}>
                      Version {entry.version}
                    </option>
                  ))
                : null}
            </SelectField>
          </div>
        )}
        {segmentId === "" ? (
          segments.status === "ready" && segments.data.length === 0 ? (
            <EmptyState title="No segments yet">Create a segment to see why a person is in it.</EmptyState>
          ) : (
            <p className="muted">Choose a segment to see why this person is, or is not, in it.</p>
          )
        ) : explain.status === "loading" ? (
          <LoadingState label="Working out why" />
        ) : explain.status === "error" ? (
          <Failure error={explain.error} onRetry={explain.reload} />
        ) : explain.data ? (
          <>
            <Summary
              explain={explain.data}
              workspaceId={workspaceId}
              currentVersion={segments.status === "ready" ? (segments.data.find((entry) => entry.id === explain.data?.segment_id)?.current_version ?? null) : null}
            />
            <ul role="tree" aria-label="Why, condition by condition" className="tree-list">
              <TreeNode node={explain.data.tree} definition={definitionOf(explain.data)} names={names} />
            </ul>
          </>
        ) : null}
      </div>
    </Panel>
  );
}
