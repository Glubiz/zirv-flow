import { useEffect, useRef, useState, type SyntheticEvent } from "react";
import { useNavigate } from "react-router";

import { ApiError, client, describeError, unwrap, type components, type DefinitionIssue, type ErrorSummary } from "../../api";
import { Button, Dialog, ErrorState, InlineError, LoadingState, Panel, SelectField, TextAreaField, TextField } from "../../ui";
import {
  fromDefinition,
  isComplete,
  place,
  samePath,
  sameDefinition,
  startsWith,
  toDefinition,
  type Node,
  type Path,
} from "./ast";
import { Builder } from "./Builder";
import type { PlacedIssue } from "./builderContext";
import { traitTypes, useCatalog, useEventTypes, type Catalog } from "./catalog";
import { asCadence, CADENCES, issueMessage, type Cadence } from "./labels";
import { LiveCount } from "./LiveCount";
import { useLeaveGuard } from "./useLeaveGuard";
import { useLivePreview } from "./useLivePreview";

export type SegmentDetail = components["schemas"]["SegmentDetailBody"];

const BLANK: Node = { all: [] };
const DEFAULT_CADENCE: Cadence = 60;

/** The names of the events a tree reads. */
function eventNames(node: Node): string[] {
  if ("all" in node) return node.all.flatMap(eventNames);
  if ("any" in node) return node.any.flatMap(eventNames);
  if ("not" in node) return eventNames(node.not);
  if ("event" in node) return [node.event];
  return [];
}

interface Issues {
  placed: PlacedIssue[];
  /** Issues whose pointer names no node of the tree; shown above the builder. */
  unplaced: DefinitionIssue[];
}

function placeIssues(root: Node, raw: readonly DefinitionIssue[]): Issues {
  const placed: PlacedIssue[] = [];
  const unplaced: DefinitionIssue[] = [];
  for (const issue of raw) {
    const where = place(root, issue.pointer);
    if (where === null) {
      unplaced.push(issue);
    } else {
      placed.push({ ...issue, path: where.path, field: where.field });
    }
  }
  return { placed, unplaced };
}

interface FormProps {
  workspaceId: string;
  /** The saved segment; null to start a new one. */
  segment: SegmentDetail | null;
  /** Whether the signed-in person may save. Without it every control is disabled. */
  canWrite: boolean;
  /** Called with the segment as saved, for the page around the form to show. */
  onSaved?: (segment: SegmentDetail) => void;
}

/** The header fields, the builder and the live count of one segment, new or saved. */
export function SegmentForm({ workspaceId, segment, canWrite, onSaved }: FormProps) {
  const catalog = useCatalog(workspaceId);
  if (catalog.status === "loading") {
    return <LoadingState label="Loading the fields, events and lists to choose from" />;
  }
  if (catalog.status === "error") {
    return (
      <Panel flush>
        <ErrorState {...describeError(catalog.error)} onRetry={catalog.reload} />
      </Panel>
    );
  }
  return <Form workspaceId={workspaceId} segment={segment} canWrite={canWrite} onSaved={onSaved} catalog={catalog.catalog} />;
}

function Form({ workspaceId, segment, canWrite, onSaved, catalog }: FormProps & { catalog: Catalog }) {
  const navigate = useNavigate();
  const [saved, setSaved] = useState(segment);
  const baseline = {
    name: saved?.name ?? "",
    description: saved?.description ?? "",
    cadence: saved?.cadence_minutes ?? DEFAULT_CADENCE,
    root: saved ? fromDefinition(saved.current.definition) : BLANK,
  };
  const [name, setName] = useState(baseline.name);
  const [description, setDescription] = useState(baseline.description);
  const [cadence, setCadence] = useState<Cadence>(baseline.cadence);
  const [root, setRoot] = useState<Node>(baseline.root);
  const [issues, setIssues] = useState<Issues>({ placed: [], unplaced: [] });
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const latestRoot = useRef(root);

  useEffect(() => {
    latestRoot.current = root;
  });

  const eventTypes = useEventTypes(workspaceId, eventNames(root));
  const complete = isComplete(root, traitTypes(catalog.traits), eventTypes);
  const definitionChanged = !sameDefinition(root, baseline.root);
  const headerChanged = name !== baseline.name || description !== baseline.description || cadence !== baseline.cadence;
  const dirty = definitionChanged || headerChanged;
  const guard = useLeaveGuard(dirty);
  const readOnly = !canWrite;

  const preview = useLivePreview(workspaceId, complete ? root : null, (raw) => {
    setIssues(placeIssues(latestRoot.current, raw));
  });

  function edit(next: Node, touched: Path, scope: "node" | "structure") {
    setRoot(next);
    setNotice(null);
    setIssues((current) => ({
      unplaced: current.unplaced,
      placed: current.placed.filter((issue) =>
        scope === "structure" ? !startsWith(issue.path, touched) : !samePath(issue.path, touched),
      ),
    }));
  }

  async function save(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    setNotice(null);
    try {
      if (saved === null) {
        const created = await unwrap(
          client.POST("/api/v1/workspaces/{ws}/segments", {
            params: { path: { ws: workspaceId } },
            body: { name: name.trim(), description, cadence_minutes: cadence, definition: toDefinition(root) },
          }),
        );
        setSaved(created);
        onSaved?.(created);
        void navigate(`/w/${workspaceId}/segments/${created.id}`);
        return;
      }
      const updated = await unwrap(
        client.PATCH("/api/v1/workspaces/{ws}/segments/{id}", {
          params: { path: { ws: workspaceId, id: saved.id } },
          body: {
            ...(name !== baseline.name ? { name: name.trim() } : {}),
            ...(description !== baseline.description ? { description } : {}),
            ...(cadence !== baseline.cadence ? { cadence_minutes: cadence } : {}),
            ...(definitionChanged ? { definition: toDefinition(root) } : {}),
          },
        }),
      );
      setSaved(updated);
      onSaved?.(updated);
      setNotice(definitionChanged ? `Saved as version ${updated.current_version}.` : "Saved.");
    } catch (error) {
      if (error instanceof ApiError && error.status === 422 && error.issues.length > 0) {
        setIssues(placeIssues(root, error.issues));
        setFailure({
          title: "The definition has problems",
          detail: "They are marked on the conditions below.",
          traceId: error.traceId,
        });
      } else {
        setFailure(describeError(error));
      }
    } finally {
      setBusy(false);
    }
  }

  const canSave = canWrite && !busy && name.trim() !== "" && complete && (saved === null || dirty);

  return (
    <form className="stack" onSubmit={(event) => void save(event)} noValidate>
      <Panel title="About this segment">
        <div className="stack">
          <TextField
            label="Name"
            value={name}
            onChange={(event) => setName(event.target.value)}
            disabled={readOnly}
            maxLength={100}
            autoComplete="off"
          />
          <TextAreaField
            label="Description"
            value={description}
            onChange={(event) => setDescription(event.target.value)}
            disabled={readOnly}
            maxLength={500}
            rows={2}
          />
          <SelectField
            label="Re-evaluate"
            value={String(cadence)}
            onChange={(event) => setCadence(asCadence(event.target.value) ?? cadence)}
            disabled={readOnly}
            hint="How often a segment that depends on the passing of time is worked out again in full."
          >
            {CADENCES.map((option) => (
              <option key={option.minutes} value={String(option.minutes)}>
                {option.label}
              </option>
            ))}
          </SelectField>
        </div>
      </Panel>
      <div className="split">
        <Panel title="Who belongs">
          <div className="stack">
            {issues.unplaced.length > 0 ? (
              <div className="form-error" role="alert">
                <span className="strong">Problems with the definition as a whole</span>
                <ul className="list-plain">
                  {issues.unplaced.map((issue) => (
                    <li key={`${issue.pointer}:${issue.code}`}>{issueMessage(issue.code)}</li>
                  ))}
                </ul>
              </div>
            ) : null}
            <Builder
              root={root}
              readOnly={readOnly}
              selfId={saved?.id ?? null}
              catalog={catalog}
              eventTypes={eventTypes}
              issues={issues.placed}
              onEdit={edit}
            />
          </div>
        </Panel>
        <LiveCount workspaceId={workspaceId} preview={preview} />
      </div>
      {failure ? <InlineError {...failure} /> : null}
      {notice ? (
        <p className="notice" role="status">
          {notice}
        </p>
      ) : null}
      <div className="row">
        {canWrite ? (
          <Button type="submit" variant="primary" disabled={!canSave}>
            {busy ? "Saving" : saved === null ? "Create segment" : "Save"}
          </Button>
        ) : (
          <p className="muted">You can look at this segment but not change it.</p>
        )}
        {dirty ? (
          <span className="pill pill-attention">
            <span className="dot dot-attention" aria-hidden="true" />
            Unsaved changes
          </span>
        ) : saved ? (
          <span className="faint">Version {saved.current_version} is saved.</span>
        ) : null}
        {canWrite && !complete ? <span className="faint">Fill in every condition to save.</span> : null}
      </div>
      {guard.pending !== null ? (
        <Dialog title="Leave without saving?" onClose={guard.stay}>
          <p className="muted">You have changes that are not saved. If you leave now, they are lost.</p>
          <div className="dialog-actions">
            <Button onClick={guard.stay}>Keep editing</Button>
            <Button variant="primary" onClick={guard.leave}>
              Leave without saving
            </Button>
          </div>
        </Dialog>
      ) : null}
    </form>
  );
}

/** A saved definition shown in the builder with every control disabled. */
export function DefinitionView({ workspaceId, definition, selfId }: { workspaceId: string; definition: Node; selfId: string }) {
  const catalog = useCatalog(workspaceId);
  const eventTypes = useEventTypes(workspaceId, eventNames(definition));
  if (catalog.status === "loading") return <LoadingState label="Loading the definition" />;
  if (catalog.status === "error") {
    return <ErrorState {...describeError(catalog.error)} onRetry={catalog.reload} />;
  }
  return (
    <Builder
      root={definition}
      readOnly
      selfId={selfId}
      catalog={catalog.catalog}
      eventTypes={eventTypes}
      issues={[]}
      onEdit={() => undefined}
    />
  );
}
