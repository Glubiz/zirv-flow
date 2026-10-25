import { useState } from "react";
import { Link, useParams } from "react-router";

import { client, describeError, unwrap, unwrapEmpty, useResource, type ErrorSummary } from "../../api";
import {
  Button,
  Dialog,
  EmptyState,
  formatDateTime,
  InlineError,
  PageHeader,
  Panel,
  Pill,
  plural,
  ResourceView,
  SelectField,
  shortId,
} from "../../ui";
import { RequireAdmin } from "../members";
import { lagText, type DeadLetter, type Destination, type QueueStatus, type Rule } from "./labels";
import { loadedKinds, useDestinationKinds } from "./kinds";
import { forwardingTargets, RuleDialog } from "./RuleDialog";

export function ForwardingPage() {
  const { ws = "" } = useParams();
  return (
    <RequireAdmin key={ws} workspaceId={ws}>
      <Forwarding workspaceId={ws} />
    </RequireAdmin>
  );
}

function Forwarding({ workspaceId }: { workspaceId: string }) {
  const destinations = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/destinations", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const status = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/forwarding/status", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  return (
    <div className="page">
      <PageHeader
        title="Forwarding"
        lede="Send the events of one name to a webhook destination, filtered and mapped to the payload you choose. Delivery is at least once; a row that keeps failing becomes a dead letter you can re-drive."
      />
      <ResourceView resource={destinations} loadingLabel="Loading destinations">
        {(known) => (
          <>
            <QueuePanel known={known} status={status} />
            <RulesPanel workspaceId={workspaceId} known={known} />
            <DeadLettersPanel workspaceId={workspaceId} known={known} onRedriven={status.reload} />
          </>
        )}
      </ResourceView>
    </div>
  );
}

function nameOf(known: Destination[], id: string): string {
  return known.find((destination) => destination.id === id)?.name ?? shortId(id);
}

function QueuePanel({ known, status }: { known: Destination[]; status: ReturnType<typeof useResource<{ destinations: QueueStatus[] }>> }) {
  return (
    <Panel title="Queues" flush>
      <ResourceView
        resource={status}
        loadingLabel="Loading the queues"
        isEmpty={(data) => data.destinations.length === 0}
        empty={<EmptyState title="Nothing is queued">No destination forwards events yet. Add a rule below.</EmptyState>}
      >
        {(data) => (
          <div className="table-scroll">
            <table className="table">
              <caption>Forwarding queue of each destination</caption>
              <thead>
                <tr>
                  <th scope="col">Destination</th>
                  <th scope="col">Waiting</th>
                  <th scope="col">Oldest lag</th>
                  <th scope="col">Dead letters</th>
                </tr>
              </thead>
              <tbody>
                {data.destinations.map((queue) => (
                  <tr key={queue.destination_id}>
                    <td className="strong">{nameOf(known, queue.destination_id)}</td>
                    <td>{queue.depth}</td>
                    <td>{lagText(queue.oldest_lag_seconds)}</td>
                    <td>
                      <Pill tone={queue.dead > 0 ? "error" : "neutral"}>{queue.dead > 0 ? `${queue.dead} dead` : "None dead"}</Pill>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </ResourceView>
    </Panel>
  );
}

function RulesPanel({ workspaceId, known }: { workspaceId: string; known: Destination[] }) {
  const rules = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/forwarding/rules", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [editing, setEditing] = useState<Rule | "new" | null>(null);
  const [deleting, setDeleting] = useState<Rule | null>(null);
  const targets = forwardingTargets(known, loadedKinds(useDestinationKinds()));

  const add = (
    <Button variant="primary" onClick={() => setEditing("new")} disabled={targets.length === 0}>
      New rule
    </Button>
  );

  return (
    <Panel title="Rules" flush>
      <ResourceView
        resource={rules}
        loadingLabel="Loading the rules"
        isEmpty={(data) => data.length === 0}
        empty={
          <EmptyState title="No forwarding rules yet" action={add}>
            {targets.length === 0 ? (
              <>
                A rule needs a webhook destination. <Link to={`/w/${workspaceId}/destinations/new`}>Add one</Link>.
              </>
            ) : (
              "A rule picks the events of one name, filters them, and maps them to the payload to deliver."
            )}
          </EmptyState>
        }
      >
        {(data) => (
          <>
            <div className="table-scroll">
              <table className="table">
                <caption>Forwarding rules</caption>
                <thead>
                  <tr>
                    <th scope="col">Name</th>
                    <th scope="col">Event</th>
                    <th scope="col">Destination</th>
                    <th scope="col">Filters</th>
                    <th scope="col">Status</th>
                    <th scope="col">
                      <span className="visually-hidden">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((rule) => (
                    <tr key={rule.id}>
                      <td className="strong">{rule.name}</td>
                      <td className="mono">{rule.event_name}</td>
                      <td>{nameOf(known, rule.destination_id)}</td>
                      <td>{rule.filter.length === 0 ? <span className="faint">All events</span> : plural(rule.filter.length, "filter", "filters")}</td>
                      <td>
                        <Pill tone={rule.active ? "neutral" : "attention"}>{rule.active ? "Active" : "Paused"}</Pill>
                      </td>
                      <td>
                        <div className="row">
                          <Button onClick={() => setEditing(rule)} aria-label={`Change ${rule.name}`}>
                            Change
                          </Button>
                          <Button onClick={() => setDeleting(rule)} aria-label={`Delete ${rule.name}`}>
                            Delete
                          </Button>
                        </div>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
            <div className="panel-footer">{add}</div>
          </>
        )}
      </ResourceView>
      {editing ? (
        <RuleDialog
          workspaceId={workspaceId}
          destinations={targets}
          existing={editing === "new" ? undefined : editing}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            rules.reload();
          }}
        />
      ) : null}
      {deleting ? (
        <DeleteRuleDialog
          workspaceId={workspaceId}
          rule={deleting}
          onClose={() => setDeleting(null)}
          onDeleted={() => {
            setDeleting(null);
            rules.reload();
          }}
        />
      ) : null}
    </Panel>
  );
}

function DeleteRuleDialog({
  workspaceId,
  rule,
  onClose,
  onDeleted,
}: {
  workspaceId: string;
  rule: Rule;
  onClose: () => void;
  onDeleted: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  async function remove() {
    setBusy(true);
    try {
      await unwrapEmpty(
        client.DELETE("/api/v1/workspaces/{ws}/forwarding/rules/{id}", { params: { path: { ws: workspaceId, id: rule.id } } }),
      );
      onDeleted();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }
  return (
    <Dialog title={`Delete ${rule.name}?`} onClose={onClose}>
      <div className="stack">
        <p className="muted">Rows the rule already queued are still delivered.</p>
        {failure ? <InlineError title={failure.title} detail={failure.detail} traceId={failure.traceId} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={() => void remove()} disabled={busy}>
            Delete the rule
          </Button>
        </div>
      </div>
    </Dialog>
  );
}

type Scope = { kind: "one"; letter: DeadLetter } | { kind: "destination"; id: string } | { kind: "all" };

function scopeText(scope: Scope, known: Destination[]): string {
  if (scope.kind === "one") return `This puts the dead letter for event ${shortId(scope.letter.event_id)} back in the queue.`;
  if (scope.kind === "destination") return `This puts every dead letter of ${nameOf(known, scope.id)} back in the queue.`;
  return "This puts every dead letter of the workspace back in the queue.";
}

const PAGE = 50;

function DeadLettersPanel({ workspaceId, known, onRedriven }: { workspaceId: string; known: Destination[]; onRedriven: () => void }) {
  const [destinationId, setDestinationId] = useState("");
  const [extra, setExtra] = useState<DeadLetter[]>([]);
  const [before, setBefore] = useState<string | null>(null);
  const [pagedOn, setPagedOn] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [more, setMore] = useState<ErrorSummary | null>(null);
  const [scope, setScope] = useState<Scope | null>(null);
  const [result, setResult] = useState<string | null>(null);
  const first = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/forwarding/dead-letters", {
          params: { path: { ws: workspaceId }, query: { limit: PAGE, ...(destinationId !== "" ? { destination_id: destinationId } : {}) } },
        }),
      ),
    [workspaceId, destinationId],
  );

  async function loadMore(cursor: string) {
    setLoadingMore(true);
    setMore(null);
    try {
      const page = await unwrap(
        client.GET("/api/v1/workspaces/{ws}/forwarding/dead-letters", {
          params: {
            path: { ws: workspaceId },
            query: { limit: PAGE, before: cursor, ...(destinationId !== "" ? { destination_id: destinationId } : {}) },
          },
        }),
      );
      setExtra((current) => [...current, ...page.letters]);
      setPagedOn(true);
      setBefore(page.next_before ?? null);
    } catch (error) {
      setMore(describeError(error));
    } finally {
      setLoadingMore(false);
    }
  }

  function filterBy(id: string) {
    setDestinationId(id);
    setExtra([]);
    setBefore(null);
    setPagedOn(false);
    setResult(null);
  }

  const cursor = pagedOn ? before : first.status === "ready" ? (first.data.next_before ?? null) : null;

  return (
    <Panel title="Dead letters" flush>
      <div className="panel-pad stack">
        <div className="row">
          <SelectField label="Destination" value={destinationId} onChange={(event) => filterBy(event.target.value)}>
            <option value="">All destinations</option>
            {known.map((destination) => (
              <option key={destination.id} value={destination.id}>
                {destination.name}
              </option>
            ))}
          </SelectField>
          {destinationId !== "" ? (
            <Button onClick={() => setScope({ kind: "destination", id: destinationId })}>Re-drive this destination</Button>
          ) : null}
          <Button onClick={() => setScope({ kind: "all" })}>Re-drive all dead letters</Button>
        </div>
        <p role="status" className="muted">
          {result}
        </p>
      </div>
      <ResourceView
        resource={first}
        loadingLabel="Loading dead letters"
        isEmpty={(data) => data.letters.length === 0}
        empty={<EmptyState title="No dead letters">Every row was delivered or is still waiting.</EmptyState>}
      >
        {(data) => {
          const letters = [...data.letters, ...extra];
          return (
            <>
              <div className="table-scroll">
                <table className="table">
                  <caption>Dead letters, newest first</caption>
                  <thead>
                    <tr>
                      <th scope="col">Died</th>
                      <th scope="col">Destination</th>
                      <th scope="col">Event</th>
                      <th scope="col">Error</th>
                      <th scope="col">Attempts</th>
                      <th scope="col">
                        <span className="visually-hidden">Actions</span>
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {letters.map((letter) => (
                      <tr key={letter.id}>
                        <td>{formatDateTime(letter.died_at)}</td>
                        <td>{nameOf(known, letter.destination_id)}</td>
                        <td className="mono">{shortId(letter.event_id)}</td>
                        <td>
                          <Pill tone="error">{letter.error ?? "unknown"}</Pill>
                        </td>
                        <td>{letter.attempts}</td>
                        <td>
                          <Button onClick={() => setScope({ kind: "one", letter })} aria-label={`Re-drive the dead letter for event ${shortId(letter.event_id)}`}>
                            Re-drive
                          </Button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              <div className="panel-footer stack-tight">
                <span className="faint">{plural(letters.length, "dead letter", "dead letters")} shown.</span>
                {more ? <InlineError title={more.title} detail={more.detail} traceId={more.traceId} /> : null}
                {cursor ? (
                  <div>
                    <Button onClick={() => void loadMore(cursor)} disabled={loadingMore}>
                      Show more
                    </Button>
                  </div>
                ) : null}
              </div>
            </>
          );
        }}
      </ResourceView>
      {scope ? (
        <RedriveDialog
          workspaceId={workspaceId}
          scope={scope}
          known={known}
          onClose={() => setScope(null)}
          onDone={(count) => {
            setScope(null);
            setResult(`${plural(count, "row", "rows")} put back in the queue.`);
            setExtra([]);
            setBefore(null);
            setPagedOn(false);
            first.reload();
            onRedriven();
          }}
        />
      ) : null}
    </Panel>
  );
}

function RedriveDialog({
  workspaceId,
  scope,
  known,
  onClose,
  onDone,
}: {
  workspaceId: string;
  scope: Scope;
  known: Destination[];
  onClose: () => void;
  onDone: (count: number) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function redrive() {
    setBusy(true);
    setFailure(null);
    try {
      const body = scope.kind === "one" ? { id: scope.letter.id } : scope.kind === "destination" ? { destination_id: scope.id } : {};
      const done = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/forwarding/re-drive", { params: { path: { ws: workspaceId } }, body }),
      );
      onDone(done.redriven);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="Re-drive dead letters?" onClose={onClose}>
      <div className="stack">
        <p>{scopeText(scope, known)}</p>
        <p className="muted">Attempts start again. A re-drive writes no event and starts no flow.</p>
        {failure ? <InlineError title={failure.title} detail={failure.detail} traceId={failure.traceId} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={() => void redrive()} disabled={busy}>
            Re-drive now
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
