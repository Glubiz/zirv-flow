import { useState } from "react";
import { Link, useParams } from "react-router";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import { Button, EmptyState, InlineError, PageHeader, Panel, Pill, ResourceView } from "../../ui";
import { useWorkspaceAccess } from "../members";
import { ConsentImportDialog } from "./ConsentImportDialog";
import { ConsentNav } from "./ConsentNav";
import { DimensionDialog, ValueDialog, type Dimension, type DimensionValue } from "./DimensionDialog";

export function DimensionsPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);
  const canManage = access.status === "ready" && access.data.canManage;

  return (
    <div className="page">
      <ConsentNav workspaceId={ws} />
      <PageHeader
        title="Consent dimensions"
        lede="What consent is recorded on besides the channel. A topic is the built-in dimension; add your own, such as a product vertical or a purpose, and give each its values. A choice that leaves a dimension out covers all of its values."
      />
      <DimensionList key={ws} workspaceId={ws} canManage={canManage} />
    </div>
  );
}

type Editing =
  | { kind: "dimension"; dimension?: Dimension }
  | { kind: "value"; dimension: Dimension; value?: DimensionValue }
  | { kind: "import" };

function DimensionList({ workspaceId, canManage }: { workspaceId: string; canManage: boolean }) {
  const [showArchived, setShowArchived] = useState(false);
  const dimensions = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/consent/dimensions", {
          params: { path: { ws: workspaceId }, query: { archived: showArchived } },
        }),
      ),
    [workspaceId, showArchived],
  );
  const [editing, setEditing] = useState<Editing | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  async function change(label: string, run: () => Promise<unknown>) {
    setBusy(label);
    setFailure(null);
    try {
      await run();
      dimensions.reload();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(null);
    }
  }

  function archiveDimension(dimension: Dimension, archived: boolean) {
    return change(`dimension:${dimension.key}`, () =>
      unwrap(
        client.PATCH("/api/v1/workspaces/{ws}/consent/dimensions/{key}", {
          params: { path: { ws: workspaceId, key: dimension.key } },
          body: { archived },
        }),
      ),
    );
  }

  function archiveValue(dimension: Dimension, value: DimensionValue, archived: boolean) {
    return change(`value:${dimension.key}:${value.key}`, () =>
      unwrap(
        client.PATCH("/api/v1/workspaces/{ws}/consent/dimensions/{key}/values/{value}", {
          params: { path: { ws: workspaceId, key: dimension.key, value: value.key } },
          body: { archived },
        }),
      ),
    );
  }

  function saved() {
    setEditing(null);
    dimensions.reload();
  }

  return (
    <>
      <div className="page-actions">
        {canManage ? (
          <>
            <Button variant="primary" onClick={() => setEditing({ kind: "dimension" })}>
              Add dimension
            </Button>
            <Button onClick={() => setEditing({ kind: "import" })}>Import consent</Button>
          </>
        ) : null}
        <label className="check">
          <input type="checkbox" checked={showArchived} onChange={(event) => setShowArchived(event.target.checked)} />
          <span>Show archived</span>
        </label>
      </div>
      {failure ? <InlineError {...failure} /> : null}
      {notice ? (
        <p className="notice" role="status">
          {notice}
        </p>
      ) : null}
      <ResourceView
        resource={dimensions}
        loadingLabel="Loading dimensions"
        isEmpty={(data) => data.length === 0}
        empty={
          <Panel flush>
            <EmptyState title="No dimensions">Dimensions appear here once the workspace has them.</EmptyState>
          </Panel>
        }
      >
        {(data) => (
          <div className="stack">
            {data.map((dimension) => (
              <DimensionCard
                key={dimension.key}
                workspaceId={workspaceId}
                dimension={dimension}
                canManage={canManage}
                busy={busy}
                onRename={() => setEditing({ kind: "dimension", dimension })}
                onArchive={(archived) => void archiveDimension(dimension, archived)}
                onAddValue={() => setEditing({ kind: "value", dimension })}
                onRenameValue={(value) => setEditing({ kind: "value", dimension, value })}
                onArchiveValue={(value, archived) => void archiveValue(dimension, value, archived)}
              />
            ))}
          </div>
        )}
      </ResourceView>
      {!canManage ? <p className="muted">Only workspace admins and the instance owner can add or change dimensions.</p> : null}
      {editing?.kind === "dimension" ? (
        <DimensionDialog workspaceId={workspaceId} dimension={editing.dimension} onClose={() => setEditing(null)} onSaved={saved} />
      ) : null}
      {editing?.kind === "value" ? (
        <ValueDialog workspaceId={workspaceId} dimension={editing.dimension} value={editing.value} onClose={() => setEditing(null)} onSaved={saved} />
      ) : null}
      {editing?.kind === "import" ? (
        <ConsentImportDialog
          workspaceId={workspaceId}
          dimensions={dimensions.status === "ready" ? dimensions.data : []}
          onClose={() => setEditing(null)}
          onImported={(message) => {
            setEditing(null);
            setNotice(message);
          }}
        />
      ) : null}
    </>
  );
}

interface CardProps {
  workspaceId: string;
  dimension: Dimension;
  canManage: boolean;
  busy: string | null;
  onRename: () => void;
  onArchive: (archived: boolean) => void;
  onAddValue: () => void;
  onRenameValue: (value: DimensionValue) => void;
  onArchiveValue: (value: DimensionValue, archived: boolean) => void;
}

function ArchiveToggle({ name, archived, busy, onArchive }: { name: string; archived: boolean; busy: boolean; onArchive: (archived: boolean) => void }) {
  const verb = archived ? "Restore" : "Archive";
  return (
    <Button onClick={() => onArchive(!archived)} disabled={busy} aria-label={`${verb} ${name}`}>
      {verb}
    </Button>
  );
}

function DimensionCard({ workspaceId, dimension, canManage, busy, onRename, onArchive, onAddValue, onRenameValue, onArchiveValue }: CardProps) {
  const editable = canManage && !dimension.builtin;
  return (
    <Panel title={dimension.name}>
      <div className="stack">
        <div className="row">
          <span className="mono faint">{dimension.key}</span>
          {dimension.builtin ? <Pill>Built in</Pill> : null}
          {dimension.archived ? <Pill tone="error">Archived</Pill> : null}
          {editable ? (
            <>
              <Button onClick={onRename} aria-label={`Rename ${dimension.name}`}>
                Rename
              </Button>
              <ArchiveToggle name={dimension.name} archived={dimension.archived} busy={busy === `dimension:${dimension.key}`} onArchive={onArchive} />
            </>
          ) : null}
        </div>
        {dimension.builtin ? (
          <p className="muted">
            Its values are the topics of the workspace. Add and change them under <Link to={`/w/${workspaceId}/topics`}>Topics</Link>.
          </p>
        ) : null}
        {dimension.values.length === 0 ? (
          <p className="faint">No values yet.</p>
        ) : (
          <ul className="list-plain stack-tight" aria-label={`Values of ${dimension.name}`}>
            {dimension.values.map((value) => (
              <li className="row" key={value.key}>
                <span className="strong">{value.name}</span>
                <span className="mono faint">{value.key}</span>
                {value.archived ? <Pill tone="error">Archived</Pill> : null}
                {editable ? (
                  <>
                    <Button onClick={() => onRenameValue(value)} aria-label={`Rename ${value.name}`}>
                      Rename
                    </Button>
                    <ArchiveToggle
                      name={value.name}
                      archived={value.archived}
                      busy={busy === `value:${dimension.key}:${value.key}`}
                      onArchive={(archived) => onArchiveValue(value, archived)}
                    />
                  </>
                ) : null}
              </li>
            ))}
          </ul>
        )}
        {editable && !dimension.archived ? (
          <div>
            <Button onClick={onAddValue} aria-label={`Add a value to ${dimension.name}`}>
              Add value
            </Button>
          </div>
        ) : null}
      </div>
    </Panel>
  );
}
