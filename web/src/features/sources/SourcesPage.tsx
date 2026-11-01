import { Link, useParams } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { EmptyState, formatDateTime, PageHeader, Panel, Pill, ResourceView } from "../../ui";
import { useSourceAccess } from "./access";
import { FreshnessBadge } from "./FreshnessBadge";
import { kindName, plural } from "./labels";

export function SourcesPage() {
  const { ws = "" } = useParams();
  return <Sources key={ws} workspaceId={ws} />;
}

function NewSourceLink({ workspaceId }: { workspaceId: string }) {
  return (
    <Link className="btn btn-primary" to={`/w/${workspaceId}/sources/new`}>
      New source
    </Link>
  );
}

function Sources({ workspaceId }: { workspaceId: string }) {
  const { canManage } = useSourceAccess(workspaceId);
  const sources = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/sources", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const kinds = useResource(() => unwrap(client.GET("/api/v1/source-kinds")), []);
  const known = kinds.status === "ready" ? kinds.data : null;

  return (
    <div className="page">
      <PageHeader
        title="Sources" breadcrumb={{ group: "Data", section: "Sources" }}
        lede="The systems your people and their data come from. A source holds its connection settings and an encrypted credential, and offers streams you choose to read."
        actions={canManage ? <NewSourceLink workspaceId={workspaceId} /> : null}
      />
      <Panel flush>
        <ResourceView
          resource={sources}
          loadingLabel="Loading sources"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState title="No sources yet" action={canManage ? <NewSourceLink workspaceId={workspaceId} /> : null}>
              Connect a source, test it, and choose the streams to read. Credentials are stored encrypted and never shown
              again.
            </EmptyState>
          }
        >
          {(data) => (
            <>
              <div className="table-scroll">
                <table className="table">
                  <caption>Sources of this workspace</caption>
                  <thead>
                    <tr>
                      <th scope="col">Name</th>
                      <th scope="col">Type</th>
                      <th scope="col">Status</th>
                      <th scope="col">Freshness</th>
                      <th scope="col">Updated</th>
                    </tr>
                  </thead>
                  <tbody>
                    {data.map((source) => (
                      <tr key={source.id}>
                        <td className="strong">
                          <Link to={`/w/${workspaceId}/sources/${source.id}`}>{source.name}</Link>
                        </td>
                        <td>{kindName(known, source.kind)}</td>
                        <td>
                          <Pill tone={source.enabled ? "neutral" : "attention"}>
                            {source.enabled ? "Enabled" : "Disabled"}
                          </Pill>
                        </td>
                        <td>
                          {source.freshness ? (
                            <FreshnessBadge state={source.freshness} />
                          ) : (
                            <span className="faint">No enabled streams</span>
                          )}
                        </td>
                        <td>{formatDateTime(source.updated_at)}</td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              <p className="panel-footer faint">{plural(data.length, "source", "sources")} in all.</p>
            </>
          )}
        </ResourceView>
      </Panel>
      {!canManage && sources.status === "ready" ? (
        <p className="muted">You can look at sources but not change them.</p>
      ) : null}
    </div>
  );
}
