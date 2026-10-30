import { Link, useParams } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { EmptyState, PageHeader, Panel, Pill, ResourceView } from "../../ui";
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
      <PageHeader title="Home" lede={current?.name} />
      <ResourceView resource={access} loadingLabel="Loading workspace access">
        {(data) => <Home key={ws} workspaceId={ws} canAdmin={data.canAdmin} />}
      </ResourceView>
    </div>
  );
}

function Home({ workspaceId, canAdmin }: { workspaceId: string; canAdmin: boolean }) {
  const resource = useResource(() => loadHome(workspaceId, canAdmin), [workspaceId, canAdmin]);
  const base = `/w/${workspaceId}`;
  return (
    <ResourceView resource={resource} loadingLabel="Loading Home">
      {({ sources, flows, setup }) => {
        const live = flows.filter((flow) => flow.state === "published");
        const unverified = setup?.domains.filter((domain) => domain.status !== "verified") ?? [];
        return (
          <>
            {setup ? <SetupChecklist base={base} data={{ ...setup, sources, flows }} /> : null}
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
                    <ul className="home-records">{live.map((flow) => (
                      <li key={flow.id}><Link to={`${base}/flows/${flow.id}`}>{flow.name}</Link><Pill>Live</Pill></li>
                    ))}</ul>
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
