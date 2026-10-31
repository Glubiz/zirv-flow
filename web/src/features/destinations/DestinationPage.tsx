import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";

import { ApiError, client, describeError, unwrap, unwrapEmpty, useResource, type ErrorSummary } from "../../api";
import { Button, Dialog, formatDateTime, InlineError, LoadingState, PageHeader, Panel, Pill, ResourceView, SelectField } from "../../ui";
import { RequireAdmin } from "../members";
import { DestinationForm } from "./DestinationForm";
import { loadedKinds, useDestinationKinds } from "./kinds";
import {
  checkCodeText,
  kindLabel,
  lagText,
  providerStatusText,
  type CheckResult,
  type Destination,
  type DestinationStats,
  type DomainStatus,
} from "./labels";

export function DestinationPage() {
  const { ws = "", id = "" } = useParams();
  return (
    <RequireAdmin key={`${ws}/${id}`} workspaceId={ws}>
      <Loaded workspaceId={ws} id={id} />
    </RequireAdmin>
  );
}

function Loaded({ workspaceId, id }: { workspaceId: string; id: string }) {
  const destination = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/destinations/{id}", { params: { path: { ws: workspaceId, id } } })),
    [workspaceId, id],
  );
  return (
    <div className="page">
      <ResourceView resource={destination} loadingLabel="Loading the destination">
        {(data) => <DestinationDetail workspaceId={workspaceId} destination={data} onChanged={destination.reload} />}
      </ResourceView>
    </div>
  );
}

function DestinationDetail({
  workspaceId,
  destination,
  onChanged,
}: {
  workspaceId: string;
  destination: Destination;
  onChanged: () => void;
}) {
  const [saved, setSaved] = useState(false);
  const kinds = loadedKinds(useDestinationKinds());
  return (
    <>
      <PageHeader
        title={destination.name}
        breadcrumb={{ group: "Sending", section: "Destinations", to: `/w/${workspaceId}/destinations` }}
        lede={`${kindLabel(kinds, destination.kind)}. ${destination.archived ? "Archived: read-only history." : "Change its settings, check it, and watch its queue."}`}
        actions={destination.archived ? <Pill tone="attention">Archived</Pill> : null}
      />
      {destination.archived ? null : (
        <>
          <Panel title="Settings">
            <DestinationForm
              key={destination.updated_at}
              workspaceId={workspaceId}
              existing={destination}
              onSaved={() => {
                setSaved(true);
                onChanged();
              }}
            />
            {saved ? (
              <p className="muted" role="status">
                Saved.
              </p>
            ) : null}
          </Panel>
          <CheckPanel workspaceId={workspaceId} destination={destination} />
        </>
      )}
      <DeliveryPanel workspaceId={workspaceId} destination={destination} />
      {destination.kind === "provider" && !destination.archived ? (
        <ProviderDomainPanel workspaceId={workspaceId} destination={destination} />
      ) : null}
      {destination.archived ? null : <ArchivePanel workspaceId={workspaceId} destination={destination} />}
    </>
  );
}

function CheckPanel({ workspaceId, destination }: { workspaceId: string; destination: Destination }) {
  const [busy, setBusy] = useState(false);
  const [result, setResult] = useState<CheckResult | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function check() {
    setBusy(true);
    setFailure(null);
    setResult(null);
    try {
      setResult(
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/destinations/{id}/check", {
            params: { path: { ws: workspaceId, id: destination.id } },
          }),
        ),
      );
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Panel title="Check">
      <div className="stack">
        <p className="muted">Tries the settings and the stored credential without delivering anything.</p>
        <div className="row">
          <Button onClick={() => void check()} disabled={busy}>
            {busy ? "Checking" : "Check the destination"}
          </Button>
        </div>
        <div role="status">
          {result ? (
            <p>
              <Pill tone={result.ok ? "neutral" : "error"}>{result.ok ? "Check passed" : "Check failed"}</Pill>{" "}
              {result.ok ? null : <span>{result.error ?? checkCodeText(result.code)}</span>}
              {!result.ok && result.code ? <span className="faint mono"> ({result.code})</span> : null}
            </p>
          ) : null}
        </div>
        {failure ? <InlineError title={failure.title} detail={failure.detail} traceId={failure.traceId} /> : null}
      </div>
    </Panel>
  );
}

function DeliveryPanel({ workspaceId, destination }: { workspaceId: string; destination: Destination }) {
  const status = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/forwarding/status", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  return (
    <Panel title="Delivery">
      <ResourceView resource={status} loadingLabel="Loading the delivery queue">
        {(data) => {
          const mine = data.destinations.find((entry) => entry.destination_id === destination.id);
          return (
            <div className="stack">
              <dl className="facts">
                <div className="fact">
                  <dt>Queue depth</dt>
                  <dd>{mine ? mine.depth : 0}</dd>
                </div>
                <div className="fact">
                  <dt>Oldest lag</dt>
                  <dd>{lagText(mine?.oldest_lag_seconds ?? 0)}</dd>
                </div>
                <div className="fact">
                  <dt>Dead letters</dt>
                  <dd>{mine ? mine.dead : 0}</dd>
                </div>
                <div className="fact">
                  <dt>Rate and burst</dt>
                  <dd>
                    {destination.rate} per second, up to {destination.burst} at once
                  </dd>
                </div>
              </dl>
              <AttemptsStats workspaceId={workspaceId} destinationId={destination.id} />
              {mine && mine.dead > 0 ? <Link to={`/w/${workspaceId}/forwarding`}>Open the dead letters</Link> : null}
            </div>
          );
        }}
      </ResourceView>
    </Panel>
  );
}

const OUTCOMES = ["delivered", "retryable", "terminal", "throttled"] as const;
const STATUS_CLASSES = ["none", "2xx", "3xx", "4xx", "5xx"] as const;
const STATS_HOURS = 24;

function AttemptsStats({ workspaceId, destinationId }: { workspaceId: string; destinationId: string }) {
  const stats = useResource<DestinationStats>(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/destinations/{id}/stats", {
          params: { path: { ws: workspaceId, id: destinationId }, query: { hours: STATS_HOURS } },
        }),
      ),
    [workspaceId, destinationId],
  );
  return (
    <section className="stack" aria-label="Delivery attempts, last 24 hours">
      <h3 className="strong">Attempts in the last 24 hours</h3>
      <ResourceView
        resource={stats}
        loadingLabel="Loading the attempts"
        isEmpty={(data) => data.attempts === 0}
        empty={<p className="muted">No attempts in the last 24 hours.</p>}
      >
        {(data) => (
          <dl className="facts">
            <div className="fact">
              <dt>Attempts</dt>
              <dd>{data.attempts}</dd>
            </div>
            {OUTCOMES.map((outcome) => (
              <div className="fact" key={outcome}>
                <dt>{outcome[0]?.toUpperCase()}{outcome.slice(1)}</dt>
                <dd>{data.outcomes[outcome]}</dd>
              </div>
            ))}
            {STATUS_CLASSES.map((statusClass) => (
              <div className="fact" key={statusClass}>
                <dt>{statusClass === "none" ? "No answer" : `Answered ${statusClass}`}</dt>
                <dd>{data.status_classes[statusClass]}</dd>
              </div>
            ))}
          </dl>
        )}
      </ResourceView>
    </section>
  );
}

function ProviderDomainPanel({ workspaceId, destination }: { workspaceId: string; destination: Destination }) {
  const domains = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/sending-domains", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [domainId, setDomainId] = useState("");
  return (
    <Panel title="Domain status at the provider">
      <ResourceView
        resource={domains}
        loadingLabel="Loading sending domains"
        isEmpty={(data) => data.length === 0}
        empty={<p className="muted">Add a sending domain first; the provider reports on it here.</p>}
      >
        {(data) => (
          <div className="stack">
            <SelectField
              label="Sending domain"
              value={domainId}
              onChange={(event) => setDomainId(event.target.value)}
              hint="Shows what the provider reports next to our own DNS check. Only our DNS check verifies a domain."
            >
              <option value="">Choose a domain</option>
              {data.map((domain) => (
                <option key={domain.id} value={domain.id}>
                  {domain.domain}
                </option>
              ))}
            </SelectField>
            {domainId !== "" ? <ProviderDomainStatus key={domainId} workspaceId={workspaceId} destinationId={destination.id} domainId={domainId} /> : null}
          </div>
        )}
      </ResourceView>
    </Panel>
  );
}

function ProviderDomainStatus({
  workspaceId,
  destinationId,
  domainId,
}: {
  workspaceId: string;
  destinationId: string;
  domainId: string;
}) {
  const status = useResource<DomainStatus | "unsupported">(async () => {
    try {
      return await unwrap(
        client.GET("/api/v1/workspaces/{ws}/destinations/{id}/domain-status", {
          params: { path: { ws: workspaceId, id: destinationId }, query: { domain_id: domainId } },
        }),
      );
    } catch (error) {
      if (error instanceof ApiError && error.code === "domain_status_unsupported") return "unsupported";
      throw error;
    }
  }, [workspaceId, destinationId, domainId]);

  if (status.status === "loading") return <LoadingState label="Asking the provider" />;
  return (
    <ResourceView resource={status} loadingLabel="Asking the provider">
      {(data) =>
        data === "unsupported" ? (
          <p className="muted">This provider&apos;s manifest declares no domain-status request.</p>
        ) : (
          <div className="stack">
            <p>
              Our DNS check: <Pill tone={data.status === "verified" ? "neutral" : "attention"}>{data.status === "verified" ? "Verified" : "Not verified"}</Pill>
            </p>
            <div className="table-scroll">
              <table className="table">
                <caption>Records of {data.domain}: our status and the provider&apos;s</caption>
                <thead>
                  <tr>
                    <th scope="col">Record</th>
                    <th scope="col">Host</th>
                    <th scope="col">Ours</th>
                    <th scope="col">Provider</th>
                  </tr>
                </thead>
                <tbody>
                  {data.records.map((record) => {
                    const theirs = record.provider ? providerStatusText(record.provider.status) : null;
                    return (
                      <tr key={`${record.kind}-${record.host}`}>
                        <td className="strong">{record.kind.toUpperCase()}</td>
                        <td className="mono">{record.host}</td>
                        <td>{record.status}</td>
                        <td>{theirs ? <Pill tone={theirs.tone}>{theirs.text}</Pill> : <span className="faint">Not reported</span>}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
            {data.provider_only.length > 0 ? (
              <ul className="list-plain" aria-label="Records only the provider reports">
                {data.provider_only.map((record) => (
                  <li key={`${record.record_type}-${record.name}`}>
                    <span className="mono">{record.name}</span> ({record.record_type}): {providerStatusText(record.status).text}
                  </li>
                ))}
              </ul>
            ) : null}
          </div>
        )
      }
    </ResourceView>
  );
}

function ArchivePanel({ workspaceId, destination }: { workspaceId: string; destination: Destination }) {
  const navigate = useNavigate();
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function archive() {
    setBusy(true);
    setFailure(null);
    try {
      await unwrapEmpty(
        client.DELETE("/api/v1/workspaces/{ws}/destinations/{id}", { params: { path: { ws: workspaceId, id: destination.id } } }),
      );
      void navigate(`/w/${workspaceId}/destinations`);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Panel title="Archive">
      <div className="stack">
        <p className="muted">
          An archived destination stops being listed, changed or checked, and keeps its delivery history. Created{" "}
          {formatDateTime(destination.created_at)}.
        </p>
        <div className="row">
          <Button onClick={() => setOpen(true)}>Archive the destination</Button>
        </div>
      </div>
      {open ? (
        <Dialog title={`Archive ${destination.name}?`} onClose={() => setOpen(false)}>
          <div className="stack">
            <p className="muted">It stops delivering. Its history stays.</p>
            {failure ? <InlineError title={failure.title} detail={failure.detail} traceId={failure.traceId} /> : null}
            <div className="dialog-actions">
              <Button onClick={() => setOpen(false)}>Cancel</Button>
              <Button variant="primary" onClick={() => void archive()} disabled={busy}>
                Archive
              </Button>
            </div>
          </div>
        </Dialog>
      ) : null}
    </Panel>
  );
}
