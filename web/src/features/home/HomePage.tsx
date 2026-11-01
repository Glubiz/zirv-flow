import { Link, useParams } from "react-router";

import { client, unwrap, useResource, type Resource } from "../../api";
import { Button, EmptyState, formatCount, InlineError, LoadingState, PageHeader, Panel, Pill, ResourceView } from "../../ui";
import { useWorkspaceAccess } from "../members";
import { useCurrentWorkspace } from "../workspaces";
import { setupChecklist, type SetupFacts } from "./checklist";

async function loadHome(ws: string, canAdmin: boolean) {
  const params = { path: { ws } };
  const [sources, flows, setup] = await Promise.all([
    unwrap(client.GET("/api/v1/workspaces/{ws}/sources", { params })),
    unwrap(client.GET("/api/v1/workspaces/{ws}/flows", { params })),
    canAdmin ? Promise.all([
      unwrap(client.GET("/api/v1/workspaces/{ws}/sending-domains", { params })),
      unwrap(client.GET("/api/v1/workspaces/{ws}/destinations", { params })),
      unwrap(client.GET("/api/v1/workspaces/{ws}/templates", { params })),
      unwrap(client.GET("/api/v1/workspaces/{ws}/segments", { params })),
    ]).then(([domains, destinations, templates, segments]) => ({ domains, destinations, templates, segments })) : null,
  ]);
  return { sources, flows, setup };
}

const WINDOW_DAYS = 7;

async function loadSummary(ws: string) {
  return unwrap(client.GET("/api/v1/workspaces/{ws}/summary", { params: { path: { ws }, query: { days: WINDOW_DAYS } } }));
}

type Summary = Awaited<ReturnType<typeof loadSummary>>;

function figureText(value: number | null | undefined, missing: string): string {
  return value === null || value === undefined ? missing : formatCount(value);
}

const TILES = [
  { label: "Messages sent", route: "flows", value: (summary: Summary) => summary.messages_sent },
  { label: "Goals reached", route: "flows", value: (summary: Summary) => summary.goals_reached },
  { label: "New people", route: "people", value: (summary: Summary) => summary.people_created },
  { label: "Events received", route: "events", value: (summary: Summary) => summary.events_received },
] as const;

export function Figures({ resource, base }: { resource: Resource<Summary>; base: string }) {
  return (
    <section className="home-tiles" aria-label={`Last ${WINDOW_DAYS} days`}>
      {resource.status === "loading" ? <LoadingState label="Loading figures" /> : null}
      {resource.status === "error" ? (
        <div className="home-tiles-error">
          <InlineError title="Could not load figures" />
          <Button onClick={resource.reload}>Try again</Button>
        </div>
      ) : null}
      {resource.status === "ready" ? TILES.map((tile) => {
        const text = figureText(tile.value(resource.data), "No access");
        return (
          <Link key={tile.label} className="panel panel-pad home-tile" to={`${base}/${tile.route}`} aria-label={`${tile.label}, ${text}, last ${WINDOW_DAYS} days`}>
            <span className="home-tile-label">{tile.label}</span>
            <span className="home-tile-value">{text}</span>
          </Link>
        );
      }) : null}
    </section>
  );
}

export function SetupChecklist({ data, base }: { data: SetupFacts; base: string }) {
  const steps = setupChecklist(data);
  const done = steps.filter((step) => step.done).length;
  if (done === steps.length) return null;
  return (
    <section className="home-setup" aria-labelledby="setup-title">
      <div className="home-setup-heading">
        <h2 id="setup-title">Set up your workspace</h2>
        <span>{done} of {steps.length} complete</span>
      </div>
      <progress value={done} max={steps.length} aria-label="Workspace setup" />
      <ol className="home-checklist">
        {steps.map((step) => (
          <li key={step.label}>
            <Link to={`${base}/${step.route}`} aria-label={`${step.label}: ${step.done ? "Done" : "Not done"}`}>
              <span aria-hidden="true">{step.done ? "✓" : "○"}</span>
              <span>{step.label}</span>
              <span className="visually-hidden">: {step.done ? "Done" : "Not done"}</span>
            </Link>
          </li>
        ))}
      </ol>
    </section>
  );
}

export function HomePage() {
  const { ws = "" } = useParams();
  const current = useCurrentWorkspace();
  const access = useWorkspaceAccess(ws);
  return (
    <div className="page home-page">
      <PageHeader title="Home" lede={current ? `${current.name} · last ${WINDOW_DAYS} days` : undefined} />
      <ResourceView resource={access} loadingLabel="Loading workspace access">
        {(data) => <Home key={ws} workspaceId={ws} canAdmin={data.canAdmin} />}
      </ResourceView>
    </div>
  );
}

function Home({ workspaceId, canAdmin }: { workspaceId: string; canAdmin: boolean }) {
  const resource = useResource(() => loadHome(workspaceId, canAdmin), [workspaceId, canAdmin]);
  const summary = useResource(() => loadSummary(workspaceId), [workspaceId]);
  const base = `/w/${workspaceId}`;
  return (
    <ResourceView resource={resource} loadingLabel="Loading Home">
      {({ sources, flows, setup }) => {
        const live = flows.filter((flow) => flow.state === "published");
        const figures = summary.status === "ready" ? new Map(summary.data.flows.map((flow) => [flow.flow_id, flow])) : null;
        const unverified = setup?.domains.filter((domain) => domain.status !== "verified") ?? [];
        return (
          <>
            {setup ? <SetupChecklist base={base} data={{ ...setup, sources, flows }} /> : null}
            <Figures resource={summary} base={base} />
            <div className="home-columns">
              <div className="stack">
                <Panel title="Needs attention">
                  {!setup ? <EmptyState title="Sending domain checks need admin access">Workspace admins can review sending domains and the setup checklist.</EmptyState>
                    : unverified.length === 0 ? <EmptyState title="No sending domains need attention" />
                      : <ul className="home-records">{unverified.map((domain) => (
                        <li key={domain.id}><Link to={`${base}/sending-domains/${domain.id}`}>{domain.domain}</Link><Pill tone="attention">Not verified</Pill></li>
                      ))}</ul>}
                </Panel>
                <Panel title="Running flows">
                  {live.length === 0 ? <EmptyState title="No running flows">Published flows appear here while they are live.</EmptyState> : (
                    <div className="table-scroll">
                      <table className="table">
                        <caption>Running flows</caption>
                        <thead><tr><th scope="col">Flow</th><th scope="col">In flow</th><th scope="col">Sent</th><th scope="col">Goal reached</th></tr></thead>
                        <tbody>{live.map((flow) => {
                          const row = figures?.get(flow.id);
                          return (
                            <tr key={flow.id}>
                              <td><Link to={`${base}/flows/${flow.id}`}>{flow.name}</Link> <Pill>Live</Pill></td>
                              <td>{figureText(row?.people_in_flow, "—")}</td>
                              <td>{figureText(row?.sent, "—")}</td>
                              <td>{figureText(row?.goals_reached, "—")}</td>
                            </tr>
                          );
                        })}</tbody>
                      </table>
                    </div>
                  )}
                  <Link className="home-all" to={`${base}/flows`}>All flows</Link>
                </Panel>
              </div>
              <Panel title="Data coming in">
                {sources.length === 0 ? <EmptyState title="No sources yet">Connect a source to bring in people and events.</EmptyState> : (
                  <ul className="home-records">{sources.map((source) => (
                    <li key={source.id}><Link to={`${base}/sources/${source.id}`}>{source.name}</Link><Pill tone={source.enabled ? "neutral" : "attention"}>{source.enabled ? "Enabled" : "Disabled"}</Pill></li>
                  ))}</ul>
                )}
                <Link className="home-all" to={`${base}/sources`}>All sources</Link>
              </Panel>
            </div>
          </>
        );
      }}
    </ResourceView>
  );
}
