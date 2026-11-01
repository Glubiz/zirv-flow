import { Link, useNavigate, useParams, useSearchParams } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { Button, EmptyState, formatAgo, PageHeader, Panel, ResourceView, TabNav } from "../../ui";
import { useSegmentAccess } from "./access";
import { cadenceLabel } from "./labels";

function matches(segment: { name: string; description?: string | null }, query: string): boolean {
  const term = query.trim().toLowerCase();
  return term === "" || `${segment.name} ${segment.description ?? ""}`.toLowerCase().includes(term);
}

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
  const [params, setParams] = useSearchParams();
  const query = params.get("q") ?? "";
  const shown = segments.status === "ready" ? segments.data.filter((segment) => matches(segment, query)) : [];

  return (
    <div className="page">
      <PageHeader
        title="Segments"
        breadcrumb={{ group: "Audience", section: "Segments" }}
        lede="Groups of people defined by rules. They update themselves."
        primary={
          canWrite ? (
            <Button variant="primary" onClick={create}>
              New segment
            </Button>
          ) : null
        }
      />
      {segments.status === "ready" && segments.data.length > 0 ? (
        <div className="row list-bar">
          <TabNav label="Filter segments" param="filter" tabs={[{ id: "all", label: "All", count: segments.data.length }]} />
          <div className="field">
            <input
              className="input"
              type="search"
              aria-label="Search segments"
              placeholder="Search segments"
              value={query}
              onChange={(event) =>
                setParams(
                  (current) => {
                    const next = new URLSearchParams(current);
                    if (event.target.value) next.set("q", event.target.value);
                    else next.delete("q");
                    return next;
                  },
                  { replace: true },
                )
              }
            />
          </div>
        </div>
      ) : null}
      <Panel flush>
        <ResourceView
          resource={segments}
          loadingLabel="Loading segments"
          isEmpty={(data) => data.length === 0 || shown.length === 0}
          empty={
            segments.status === "ready" && segments.data.length > 0 ? (
              <EmptyState title="No segments match">Nothing is called or described like &ldquo;{query}&rdquo;.</EmptyState>
            ) : (
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
            )
          }
        >
          {() => (
            <div className="table-scroll">
              <table className="table">
                <caption>Segments</caption>
                <thead>
                  <tr>
                    <th scope="col">Segment</th>
                    <th scope="col" className="num">
                      People
                    </th>
                    <th scope="col">Updated</th>
                    <th scope="col">Kept up to date</th>
                  </tr>
                </thead>
                <tbody>
                  {shown.map((segment) => (
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
