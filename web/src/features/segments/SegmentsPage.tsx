import { Link, useNavigate, useParams } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { Button, EmptyState, formatAgo, PageHeader, Panel, ResourceView } from "../../ui";
import { useSegmentAccess } from "./access";
import { cadenceLabel } from "./labels";

export function SegmentsPage() {
  const { ws = "" } = useParams();
  return <Segments key={ws} workspaceId={ws} />;
}

function Segments({ workspaceId }: { workspaceId: string }) {
  const navigate = useNavigate();
  const { canWrite } = useSegmentAccess(workspaceId);
  const segments = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/segments", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const create = () => void navigate(`/w/${workspaceId}/segments/new`);

  return (
    <div className="page">
      <PageHeader
        title="Segments"
        lede="Groups of people picked out by rules over what you know about them. A segment keeps itself up to date as the data changes."
        actions={
          canWrite ? (
            <Button variant="primary" onClick={create}>
              New segment
            </Button>
          ) : null
        }
      />
      <Panel flush>
        <ResourceView
          resource={segments}
          loadingLabel="Loading segments"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState
              title="No segments yet"
              action={
                canWrite ? (
                  <Button variant="primary" onClick={create}>
                    Create the first segment
                  </Button>
                ) : null
              }
            >
              A segment is a rule such as everyone who bought in the last 90 days and has not unsubscribed.
            </EmptyState>
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Segments of this workspace</caption>
                <thead>
                  <tr>
                    <th scope="col">Name</th>
                    <th scope="col" className="num">
                      People
                    </th>
                    <th scope="col">Last computed</th>
                    <th scope="col">Kept up to date</th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((segment) => (
                    <tr key={segment.id}>
                      <td className="strong">
                        <Link to={`/w/${workspaceId}/segments/${segment.id}`}>{segment.name}</Link>
                        {segment.description ? <div className="faint">{segment.description}</div> : null}
                      </td>
                      <td className="num">{segment.member_count}</td>
                      <td>{segment.computed_at ? `computed ${formatAgo(segment.computed_at)}` : "not computed yet"}</td>
                      <td>re-evaluated {cadenceLabel(segment.cadence_minutes)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
      </Panel>
      {!canWrite && segments.status === "ready" ? <p className="muted">You can look at segments but not change them.</p> : null}
    </div>
  );
}
