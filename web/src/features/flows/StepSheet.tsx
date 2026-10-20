import { Button, SelectField, Sheet } from "../../ui";
import type { Catalog } from "../segments";
import { wordsFor, type Names } from "./describe";
import { fieldError } from "./fields";
import { issueText } from "./issues";
import { NodeFields } from "./NodeFields";
import { edgeFrom, kindLabel, portLabel, portsOf, type FlowIssue, type FlowNode, type Graph } from "./model";

interface Props {
  workspaceId: string;
  graph: Graph;
  node: FlowNode;
  names: Names;
  /** Every step in reading order, for naming the choices of a connection. */
  steps: readonly { id: string; step: number }[];
  issues: readonly FlowIssue[];
  catalog: Catalog | null;
  readOnly: boolean;
  autoFocus: boolean;
  onClose: () => void;
  onChange: (next: FlowNode, key?: string) => void;
  onRemovePort: (next: FlowNode, prefix: "case_" | "arm_", index: number) => void;
  /** Connects an output to a step, or disconnects it when `to` is null. */
  onWire: (port: string, to: string | null) => void;
  onDelete: () => void;
}

/** The slide-over that edits the selected step: its issues, its fields, where each output leads. */
export function StepSheet(props: Props) {
  const { node, graph, names, steps, issues, readOnly } = props;
  const words = wordsFor(node, names);
  const ports = portsOf(node);
  const others = steps.filter((entry) => entry.id !== node.id);

  return (
    <Sheet
      title={words.title}
      eyebrow={
        <>
          {kindLabel(node.type)} · <span className="mono">{node.id}</span>
        </>
      }
      closeLabel="Close"
      onClose={props.onClose}
      autoFocus={props.autoFocus}
    >
      {issues.length > 0 ? (
        <div className="form-error" aria-label="Problems with this step" role="group">
          <span className="strong">{issues.length === 1 ? "This step has a problem" : "This step has problems"}</span>
          <ul className="list-plain">
            {issues.map((issue, index) => (
              <li key={`${issue.code}:${issue.at ?? ""}:${index}`}>{issueText(issue, graph)}</li>
            ))}
          </ul>
        </div>
      ) : null}
      {readOnly ? <p className="muted">You can look at this step but not change it.</p> : null}
      <NodeFields
        workspaceId={props.workspaceId}
        graph={graph}
        node={node}
        issues={issues}
        catalog={props.catalog}
        readOnly={readOnly}
        onChange={props.onChange}
        onRemovePort={props.onRemovePort}
      />
      {ports.length > 0 ? (
        <fieldset className="fl-fieldset">
          <legend className="field-label">Where it goes next</legend>
          {ports.map((port) => {
            const edge = edgeFrom(graph, node.id, port);
            const label = port === "next" ? "Next step" : `${portLabel(node, port)} leads to`;
            return (
              <div key={port} className="fl-wire">
                <SelectField
                  label={label}
                  value={edge?.to ?? ""}
                  onChange={(event) => props.onWire(port, event.target.value === "" ? null : event.target.value)}
                  disabled={readOnly}
                  error={fieldError(issues, `/ports/${port}`, graph)}
                >
                  <option value="">Not connected</option>
                  {others.map((entry) => {
                    const target = graph.nodes.find((candidate) => candidate.id === entry.id);
                    return (
                      <option key={entry.id} value={entry.id}>
                        {target ? `Step ${entry.step}: ${kindLabel(target.type)}, ${wordsFor(target, names).title}` : entry.id}
                      </option>
                    );
                  })}
                  {edge && !graph.nodes.some((candidate) => candidate.id === edge.to) ? <option value={edge.to}>A step that is not in the flow</option> : null}
                </SelectField>
                {!readOnly && edge ? (
                  <Button aria-label={`Disconnect ${portLabel(node, port)}`} onClick={() => props.onWire(port, null)}>
                    Disconnect
                  </Button>
                ) : null}
              </div>
            );
          })}
        </fieldset>
      ) : null}
      {readOnly ? null : (
        <div className="fl-sheet-foot">
          <Button className="fl-danger" onClick={props.onDelete}>
            Delete step…
          </Button>
        </div>
      )}
    </Sheet>
  );
}
