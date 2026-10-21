import { useMemo, useRef, useState } from "react";

import { client, unwrap, useResource, type components } from "../../api";
import { Button, EmptyState, formatAgo, formatDateTime, Panel, Pill, ResourceView } from "../../ui";
import { useMembers } from "../members";
import type { Catalog } from "../segments";
import { Canvas, type FocusRequest } from "./Canvas";
import type { Names } from "./describe";
import { FlowAssistantSheet } from "./FlowAssistant";
import { placeIssues } from "./issues";
import { layout } from "./layout";
import { TRIGGER_ID, type FlowDetail } from "./model";
import type { Overlay } from "./overlay";
import { StepSheet } from "./StepSheet";

export type FlowVersion = components["schemas"]["FlowVersionBody"];

interface VersionsViewProps {
  workspaceId: string;
  flowId: string;
  /** The state and newest version of the flow, to say which version is live. */
  state: FlowDetail["state"];
  current: number | null;
  /** Whether the person may publish; the empty state then offers it. */
  canPublish: boolean;
  /** A publish is on its way: the button says so and does nothing. */
  publishing?: boolean;
  /** A lifecycle action is on its way: the button does nothing. */
  acting?: boolean;
  onPublish: () => void;
  onView: (version: FlowVersion) => void;
}

/** The published versions of a flow, newest first. */
export function VersionsView({
  workspaceId,
  flowId,
  state,
  current,
  canPublish,
  publishing = false,
  acting = false,
  onPublish,
  onView,
}: VersionsViewProps) {
  const versions = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/flows/{id}/versions", { params: { path: { ws: workspaceId, id: flowId } } })),
    [workspaceId, flowId, current],
  );
  const members = useMembers(workspaceId);
  const emails = useMemo(
    () => new Map(members.status === "ready" ? members.data.map((member) => [member.user_id, member.email]) : []),
    [members],
  );

  function publisher(version: FlowVersion): string | null {
    if (version.published_by === null || version.published_by === undefined) return "an API key";
    return emails.get(version.published_by) ?? null;
  }

  return (
    <Panel flush>
      <ResourceView
        resource={versions}
        loadingLabel="Loading the versions"
        isEmpty={(list) => list.length === 0}
        empty={
          <EmptyState
            title="Not published yet"
            action={
              canPublish ? (
                <Button variant="primary" aria-disabled={publishing || acting || undefined} onClick={onPublish}>
                  {publishing ? "Publishing…" : "Publish…"}
                </Button>
              ) : undefined
            }
          >
            Each version you publish is kept here, and you can look at its steps.
          </EmptyState>
        }
      >
        {(list) => (
          <ul className="fl-versions" aria-label="Published versions">
            {[...list]
              .sort((left, right) => right.version - left.version)
              .map((version) => {
                const by = publisher(version);
                const live = version.version === current && (state === "published" || state === "paused");
                return (
                  <li key={version.version} className="fl-version">
                    <span className="fl-version-name">Version {version.version}</span>
                    {live ? <Pill tone={state === "paused" ? "attention" : "neutral"}>{state === "paused" ? "Current, paused" : "Live"}</Pill> : null}
                    <span className="fl-version-meta">
                      <time dateTime={version.published_at} title={formatDateTime(version.published_at)}>
                        Published {formatAgo(version.published_at)}
                      </time>
                      {by ? ` by ${by}` : ""}
                    </span>
                    <Button aria-label={`View version ${version.version}`} onClick={() => onView(version)}>
                      View
                    </Button>
                  </li>
                );
              })}
          </ul>
        )}
      </ResourceView>
    </Panel>
  );
}

interface VersionCanvasProps {
  workspaceId: string;
  flowId: string;
  /** Whether the assistant is on and the person may use it: the canvas then offers to explain this version. */
  assistantOn: boolean;
  version: FlowVersion;
  names: Names;
  catalog: Catalog | null;
}

/** One published version drawn on the editor's canvas, with nothing to change. */
export function VersionCanvas({ workspaceId, flowId, version, names, catalog, assistantOn }: VersionCanvasProps) {
  const { graph, settings } = version;
  const [selected, setSelected] = useState<string | null>(null);
  const [focusRequest, setFocusRequest] = useState<FocusRequest | null>(null);
  const [assistantOpen, setAssistantOpen] = useState(false);
  const [cited, setCited] = useState<readonly string[]>([]);
  const nonce = useRef(0);
  const none = useMemo(() => placeIssues([]), []);
  const plan = useMemo(() => layout(graph), [graph]);
  const steps = useMemo(
    () => plan.order.filter((id) => id !== TRIGGER_ID).map((id, index) => ({ id, step: index + 1 })),
    [plan],
  );
  const node = selected === null ? undefined : graph.nodes.find((candidate) => candidate.id === selected);

  const citedOverlay = useMemo<Overlay | undefined>(
    () =>
      assistantOpen && cited.length > 0
        ? { steps: new Map(cited.map((id) => [id, [] as number[]])), dimOthers: true }
        : undefined,
    [assistantOpen, cited],
  );

  function toggleAssistant() {
    if (assistantOpen && node === undefined) {
      setAssistantOpen(false);
      setCited([]);
      return;
    }
    setSelected(null);
    setAssistantOpen(true);
  }

  function reveal(id: string) {
    nonce.current += 1;
    setFocusRequest({ id, nonce: nonce.current });
  }

  function close() {
    if (selected !== null) {
      nonce.current += 1;
      setFocusRequest({ id: selected, nonce: nonce.current });
    }
    setSelected(null);
  }

  return (
    <Canvas
      graph={graph}
      settings={settings}
      names={names}
      placed={none}
      issueItems={[]}
      checked={false}
      checking={false}
      editable={false}
      selectedId={node ? node.id : null}
      focusRequest={focusRequest}
      canUndo={false}
      overlay={citedOverlay}
      assistant={assistantOn ? { open: assistantOpen, onToggle: toggleAssistant } : undefined}
      onOpen={(id) => {
        if (id !== TRIGGER_ID) setSelected(id);
      }}
      onClose={close}
      onWire={() => undefined}
      onAdd={() => undefined}
      onUndo={() => undefined}
      onDeleteRequest={() => undefined}
      onPickIssue={() => undefined}
    >
      {assistantOn && assistantOpen ? (
        <div className="fl-sheet" hidden={node !== undefined}>
          <FlowAssistantSheet
            workspaceId={workspaceId}
            flowId={flowId}
            version={version.version}
            graph={graph}
            names={names}
            onReveal={reveal}
            onCited={setCited}
            onClose={toggleAssistant}
          />
        </div>
      ) : null}
      {node ? (
        <div className="fl-sheet">
          <StepSheet
            key={node.id}
            workspaceId={workspaceId}
            graph={graph}
            node={node}
            names={names}
            steps={steps}
            issues={[]}
            catalog={catalog}
            readOnly
            autoFocus
            onClose={close}
            onChange={() => undefined}
            onRemovePort={() => undefined}
            onWire={() => undefined}
            onDelete={() => undefined}
          />
        </div>
      ) : null}
    </Canvas>
  );
}
