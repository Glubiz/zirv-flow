import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";

import { client, describeError, unwrap, unwrapEmpty, useResource, type ErrorSummary } from "../../api";
import { Button, Dialog, formatDateTime, InlineError, PageHeader, Panel, Pill, ResourceView } from "../../ui";
import { useSourceAccess } from "./access";
import { EditSourceDialog } from "./EditSourceDialog";
import { FreshnessBadge } from "./FreshnessBadge";
import { kindName, plural, showValue, type Stream } from "./labels";
import { RunsPanel } from "./RunsPanel";
import { StreamsPanel } from "./StreamsPanel";

export function SourcePage() {
  const { ws = "", sourceId = "" } = useParams();
  return <SourceDetail key={`${ws}/${sourceId}`} workspaceId={ws} sourceId={sourceId} />;
}

function SourceDetail({ workspaceId, sourceId }: { workspaceId: string; sourceId: string }) {
  const { canManage } = useSourceAccess(workspaceId);
  const navigate = useNavigate();
  const source = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/sources/{id}", { params: { path: { ws: workspaceId, id: sourceId } } }),
      ),
    [workspaceId, sourceId],
  );
  const streams = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/sources/{id}/streams", {
          params: { path: { ws: workspaceId, id: sourceId } },
        }),
      ),
    [workspaceId, sourceId],
  );
  const kinds = useResource(() => unwrap(client.GET("/api/v1/source-kinds")), []);
  const [editing, setEditing] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [runsVersion, setRunsVersion] = useState(0);

  async function run(task: () => Promise<void>) {
    setBusy(true);
    setFailure(null);
    setNotice(null);
    try {
      await task();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  const path = { params: { path: { ws: workspaceId, id: sourceId } } };

  const test = () =>
    run(async () => {
      const result = await unwrap(client.POST("/api/v1/workspaces/{ws}/sources/{id}/test", path));
      if (result.ok) {
        setNotice("The connection works.");
      } else {
        setFailure({ title: "The connection test failed", detail: result.error ?? "", traceId: null });
      }
    });

  const discover = () =>
    run(async () => {
      const found = await unwrap(client.POST("/api/v1/workspaces/{ws}/sources/{id}/discover", path));
      setNotice(`Found ${plural(found.length, "stream", "streams")}.`);
      streams.reload();
    });

  const remove = () =>
    run(async () => {
      await unwrapEmpty(client.DELETE("/api/v1/workspaces/{ws}/sources/{id}", path));
      void navigate(`/w/${workspaceId}/sources`);
    });

  /** Starts a run of one stream, or of every enabled stream that has a mapping. */
  const startRuns = (stream: string | null, kind: "sync" | "reconcile" = "sync") =>
    run(async () => {
      const started = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/sources/{id}/runs", {
          params: { path: { ws: workspaceId, id: sourceId } },
          body: stream === null ? {} : kind === "reconcile" ? { stream, kind } : { stream },
        }),
      );
      setNotice(
        kind === "reconcile"
          ? "Started a reconciliation."
          : `Started ${plural(started.length, "run", "runs")}.`,
      );
      setRunsVersion((value) => value + 1);
    });

  function streamChanged(saved: Stream) {
    setNotice(`Saved ${saved.name}.`);
    streams.reload();
  }

  return (
    <div className="page">
      <ResourceView resource={source} loadingLabel="Loading source">
        {(data) => {
          const known = kinds.status === "ready" ? kinds.data : null;
          const kind = known?.find((entry) => entry.kind === data.kind) ?? null;
          return (
            <>
              <PageHeader
                title={data.name}
                lede={`${kindName(known, data.kind)} source, updated ${formatDateTime(data.updated_at)}.`}
                actions={
                  canManage ? (
                    <>
                      <Button onClick={() => void test()} disabled={busy}>
                        Test connection
                      </Button>
                      <Button onClick={() => setEditing(true)} disabled={kind === null}>
                        Edit
                      </Button>
                      <Button
                        onClick={() => {
                          setFailure(null);
                          setDeleting(true);
                        }}
                      >
                        Delete
                      </Button>
                    </>
                  ) : null
                }
              />
              {notice ? (
                <p className="notice" role="status">
                  {notice}
                </p>
              ) : null}
              {failure && !deleting ? <InlineError {...failure} /> : null}
              <Panel title="Settings">
                <dl className="facts">
                  <div className="fact">
                    <dt>Status</dt>
                    <dd>
                      <Pill tone={data.enabled ? "neutral" : "attention"}>{data.enabled ? "Enabled" : "Disabled"}</Pill>
                    </dd>
                  </div>
                  <div className="fact">
                    <dt>Freshness</dt>
                    <dd>
                      {data.freshness ? <FreshnessBadge state={data.freshness} /> : "No enabled streams"}
                    </dd>
                  </div>
                  <div className="fact">
                    <dt>Schedule</dt>
                    <dd>{data.schedule ? <span className="mono">{data.schedule}</span> : "None"}</dd>
                  </div>
                  {Object.entries(data.config).map(([key, value]) => (
                    <div key={key} className="fact">
                      <dt>{kind?.config_fields.find((field) => field.key === key)?.label ?? key}</dt>
                      <dd>{showValue(value)}</dd>
                    </div>
                  ))}
                  {(kind?.credential_fields ?? []).map((field) => (
                    <div key={field.key} className="fact">
                      <dt>{field.label}</dt>
                      <dd>{data.credential_fields_set.includes(field.key) ? "Set" : "Not set"}</dd>
                    </div>
                  ))}
                </dl>
              </Panel>
              <Panel title="Streams" flush>
                <ResourceView resource={streams} loadingLabel="Loading streams">
                  {(list) => (
                    <>
                      <StreamsPanel
                        workspaceId={workspaceId}
                        sourceId={sourceId}
                        streams={list}
                        canManage={canManage}
                        onChanged={streamChanged}
                        onRun={(stream) => void startRuns(stream.name)}
                        onReconcile={(stream) => void startRuns(stream.name, "reconcile")}
                        busy={busy}
                      />
                      {canManage ? (
                        <div className="panel-footer">
                          <Button onClick={() => void discover()} disabled={busy}>
                            {list.length === 0 ? "Discover streams" : "Discover streams again"}
                          </Button>
                        </div>
                      ) : null}
                    </>
                  )}
                </ResourceView>
              </Panel>
              <Panel title="Runs" flush>
                {canManage ? (
                  <div className="panel-footer">
                    <Button onClick={() => void startRuns(null)} disabled={busy}>
                      Run now
                    </Button>
                  </div>
                ) : null}
                <RunsPanel workspaceId={workspaceId} sourceId={sourceId} canManage={canManage} version={runsVersion} />
              </Panel>
              {!canManage ? <p className="muted">You can look at this source but not change it.</p> : null}
              <p>
                <Link to={`/w/${workspaceId}/sources`}>Back to sources</Link>
              </p>
              {editing && kind ? (
                <EditSourceDialog
                  workspaceId={workspaceId}
                  source={data}
                  kind={kind}
                  onClose={() => setEditing(false)}
                  onSaved={() => {
                    setEditing(false);
                    setNotice(`Saved ${data.name}.`);
                    source.reload();
                  }}
                />
              ) : null}
              {deleting ? (
                <Dialog title={`Delete ${data.name}?`} onClose={() => setDeleting(false)}>
                  <p className="muted">
                    Deleting <span className="strong">{data.name}</span> removes its settings, its streams and its stored
                    credential. This cannot be undone.
                  </p>
                  {failure ? <InlineError {...failure} /> : null}
                  <div className="dialog-actions">
                    <Button onClick={() => setDeleting(false)}>Keep source</Button>
                    <Button variant="primary" onClick={() => void remove()} disabled={busy}>
                      {busy ? "Deleting" : `Delete ${data.name}`}
                    </Button>
                  </div>
                </Dialog>
              ) : null}
            </>
          );
        }}
      </ResourceView>
    </div>
  );
}
