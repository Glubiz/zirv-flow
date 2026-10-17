import { client, unwrap, useResource, type components } from "../../api";
import { Button, ErrorState, LoadingState, Panel } from "../../ui";
import { failureText } from "../assistant";
import { fromDefinition, getAt, place, type Definition } from "./ast";
import { describeNode, namesOf, type Names } from "./describe";

type Explained = components["schemas"]["AiExplainBody"];

interface Props {
  workspaceId: string;
  segmentId: string;
  /** The definition being explained: the current version's, shown in words beside the assistant's text. */
  definition: Definition;
  /** Called when the person closes the explanation. */
  onClose: () => void;
}

/**
 * The assistant's explanation of the current version: a summary and, for each condition, what it
 * says in words (the builder's own description) with the assistant's sentence. The sentences are
 * plain text and are rendered as text, never as markup. It asks on mount and again on retry.
 */
export function AssistantExplanation({ workspaceId, segmentId, definition, onClose }: Props) {
  const explained = useResource(
    () =>
      unwrap(
        client.POST("/api/v1/workspaces/{ws}/segments/{id}/ai/explain", {
          params: { path: { ws: workspaceId, id: segmentId } },
        }),
      ),
    [workspaceId, segmentId],
  );
  const lists = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/lists", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const segments = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/segments", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const names = namesOf(lists.status === "ready" ? lists.data : undefined, segments.status === "ready" ? segments.data : undefined);

  return (
    <Panel title="Explanation by the assistant">
      <div className="stack">
        <div aria-live="polite">
          {explained.status === "loading" ? (
            <LoadingState label="Asking the assistant to explain this segment. This can take a minute." />
          ) : explained.status === "error" ? (
            <ErrorState {...failureText(explained.error)} onRetry={explained.reload} />
          ) : (
            <Explanation explained={explained.data} definition={definition} names={names} />
          )}
        </div>
        <div className="row">
          <Button onClick={onClose}>Close</Button>
        </div>
      </div>
    </Panel>
  );
}

function Explanation({ explained, definition, names }: { explained: Explained; definition: Definition; names: Names }) {
  const root = fromDefinition(definition);
  return (
    <div className="stack">
      <p className="faint">
        Version {explained.version}, as written by the assistant. Check it against the conditions: the words under each one are the
        builder&rsquo;s own.
      </p>
      <p>{explained.summary}</p>
      <ol className="list-plain stack-tight" aria-label="Condition by condition">
        {explained.nodes.map((entry) => {
          const placement = place(root, entry.pointer);
          const node = placement ? getAt(root, placement.path) : undefined;
          return (
            <li key={entry.pointer} className="stack-tight">
              {node ? <span className="strong">{describeNode(node, names)}</span> : null}
              <span>{entry.text}</span>
            </li>
          );
        })}
      </ol>
    </div>
  );
}
