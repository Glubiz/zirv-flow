import { Link, useNavigate, useParams } from "react-router";

import { client, unwrap, useResource, type components } from "../../api";
import { Button, EmptyState, formatAgo, PageHeader, Panel, Pill, ResourceView, type Tone } from "../../ui";
import { useFlowAccess } from "./access";

type Flow = components["schemas"]["FlowBody"];

const STATES: Record<Flow["state"], { label: string; tone: Tone }> = {
  draft: { label: "Draft", tone: "neutral" },
  published: { label: "Live", tone: "neutral" },
  paused: { label: "Paused", tone: "attention" },
  archived: { label: "Archived", tone: "neutral" },
};

export function StatePill({ state }: { state: Flow["state"] }) {
  const { label, tone } = STATES[state];
  return <Pill tone={tone}>{label}</Pill>;
}

export function FlowsPage() {
  const { ws = "" } = useParams();
  return <Flows key={ws} workspaceId={ws} />;
}

function Flows({ workspaceId }: { workspaceId: string }) {
  const navigate = useNavigate();
  const { canWrite } = useFlowAccess(workspaceId);
  const flows = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/flows", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const create = () => void navigate(`/w/${workspaceId}/flows/new`);

  return (
    <div className="page">
      <PageHeader
        title="Flows"
        breadcrumb={{ group: "Messaging", section: "Flows" }}
        lede="Automations that start when something happens to a person and then walk them through steps: waiting, branching, updating what you know about them and calling out."
        actions={
          canWrite ? (
            <Button variant="primary" onClick={create}>
              New flow
            </Button>
          ) : null
        }
      />
      <Panel flush>
        <ResourceView
          resource={flows}
          loadingLabel="Loading flows"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState
              title="No flows yet"
              action={
                canWrite ? (
                  <Button variant="primary" onClick={create}>
                    New flow
                  </Button>
                ) : null
              }
            >
              A flow starts when a person enters a segment or an event happens, and then runs the steps you lay out.
            </EmptyState>
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Flows of this workspace</caption>
                <thead>
                  <tr>
                    <th scope="col">Name</th>
                    <th scope="col">State</th>
                    <th scope="col">Live version</th>
                    <th scope="col">Changed</th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((flow) => (
                    <tr key={flow.id}>
                      <td className="strong">
                        <Link to={`/w/${workspaceId}/flows/${flow.id}`}>{flow.name}</Link>
                      </td>
                      <td>
                        <StatePill state={flow.state} />
                      </td>
                      <td>{flow.current_version ? `Version ${flow.current_version}` : "Not published"}</td>
                      <td>{formatAgo(flow.updated_at)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
      </Panel>
      {!canWrite && flows.status === "ready" ? <p className="muted">You can look at flows but not change them.</p> : null}
    </div>
  );
}
