import { client, describeError, problemFrom, unwrap, useResource, type Resource } from "../../api";
import { Button, ErrorState, LoadingState, PageHeader, Pill } from "../../ui";
import type { ReactNode } from "react";

type Probe = { ok: true } | { ok: false; title: string; detail: string; traceId: string | null };

async function probe(path: "/healthz" | "/readyz"): Promise<Probe> {
  const { error, response } = await client.GET(path);
  if (response.ok) {
    return { ok: true };
  }
  const problem = problemFrom(error, response);
  return { ok: false, title: problem.title, detail: problem.detail, traceId: problem.traceId };
}

interface ProbeCardProps {
  heading: string;
  path: string;
  about: string;
  okWord: string;
  failWord: string;
  resource: Resource<Probe>;
}

function Card({ heading, path, about, children }: { heading: string; path: string; about: string; children: ReactNode }) {
  return (
    <section className="panel panel-pad" aria-label={heading}>
      <h2 className="panel-title">{heading}</h2>
      <p className="muted">
        <span className="mono">{path}</span> {about}
      </p>
      {children}
    </section>
  );
}

function ProbeCard({ heading, path, about, okWord, failWord, resource }: ProbeCardProps) {
  return (
    <Card heading={heading} path={path} about={about}>
      {resource.status === "loading" ? <LoadingState label={`Checking ${heading.toLowerCase()}`} /> : null}
      {resource.status === "error" ? <ErrorState {...describeError(resource.error)} onRetry={resource.reload} /> : null}
      {resource.status === "ready" && resource.data.ok ? (
        <div>
          <Pill>{okWord}</Pill>
        </div>
      ) : null}
      {resource.status === "ready" && !resource.data.ok ? (
        <div className="stack-tight" role="alert">
          <div>
            <Pill tone="error">{failWord}</Pill>
          </div>
          <p className="strong">{resource.data.title}</p>
          {resource.data.detail ? <p className="muted">{resource.data.detail}</p> : null}
          {resource.data.traceId ? (
            <p className="faint">
              Trace ID <span className="mono">{resource.data.traceId}</span>
            </p>
          ) : null}
        </div>
      ) : null}
    </Card>
  );
}

export function HealthPage() {
  const live = useResource(() => probe("/healthz"), []);
  const ready = useResource(() => probe("/readyz"), []);
  const version = useResource(() => unwrap(client.GET("/api/v1/version")), []);

  function refresh() {
    live.reload();
    ready.reload();
    version.reload();
  }

  return (
    <div className="page">
      <PageHeader
        title="Health"
        lede="Whether this instance is up and able to serve work."
        actions={<Button onClick={refresh}>Refresh</Button>}
      />
      <div className="grid-3">
        <ProbeCard
          heading="Running"
          path="/healthz"
          about="answers while the process is alive."
          okWord="Alive"
          failWord="Not running"
          resource={live}
        />
        <ProbeCard
          heading="Ready for work"
          path="/readyz"
          about="answers once the database is reachable and up to date."
          okWord="Ready"
          failWord="Not ready"
          resource={ready}
        />
        <Card heading="Version" path="/api/v1/version" about="is the release now running.">
          {version.status === "loading" ? <LoadingState label="Checking the version" /> : null}
          {version.status === "error" ? <ErrorState {...describeError(version.error)} onRetry={version.reload} /> : null}
          {version.status === "ready" ? <p className="mono">v{version.data.version}</p> : null}
        </Card>
      </div>
    </div>
  );
}
