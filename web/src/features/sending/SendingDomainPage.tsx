import { useEffect, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router";

import { client, describeError, unwrap, unwrapEmpty, useResource, type ErrorSummary } from "../../api";
import { Button, Dialog, EmptyState, formatDateTime, InlineError, Panel, Pill, ResourceView } from "../../ui";
import { RequireAdmin } from "../members";
import { IdentityDialog } from "./IdentityDialog";
import { blockedReason, verifiedText, type SenderIdentity, type SendingDomain } from "./labels";
import { RecordStep } from "./RecordStep";

/** How the DNS re-check waits for the background check, which tests shorten. */
export const timing = { recheckMs: 1500, recheckTries: 12 };

export function SendingDomainPage() {
  const { ws = "", id = "" } = useParams();
  return (
    <RequireAdmin key={`${ws}/${id}`} workspaceId={ws}>
      <Loaded workspaceId={ws} id={id} />
    </RequireAdmin>
  );
}

function Loaded({ workspaceId, id }: { workspaceId: string; id: string }) {
  const domain = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/sending-domains/{id}", { params: { path: { ws: workspaceId, id } } })),
    [workspaceId, id],
  );
  return (
    <div className="page">
      <ResourceView resource={domain} loadingLabel="Loading the sending domain">
        {(data) => <DomainView workspaceId={workspaceId} initial={data} />}
      </ResourceView>
    </div>
  );
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function DomainView({ workspaceId, initial }: { workspaceId: string; initial: SendingDomain }) {
  const navigate = useNavigate();
  const [domain, setDomain] = useState(initial);
  const [checking, setChecking] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [timedOut, setTimedOut] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const alive = useRef(true);
  const isAlive = () => alive.current;

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  async function recheck() {
    setChecking(true);
    setFailure(null);
    setTimedOut(false);
    const before = domain.last_checked_at ?? null;
    try {
      await unwrap(
        client.POST("/api/v1/workspaces/{ws}/sending-domains/{id}/verify", {
          params: { path: { ws: workspaceId, id: domain.id } },
        }),
      );
      // The check runs in the background: read the domain again until it has been checked.
      for (let attempt = 0; attempt < timing.recheckTries; attempt += 1) {
        await sleep(timing.recheckMs);
        if (!isAlive()) return;
        const fresh = await unwrap(
          client.GET("/api/v1/workspaces/{ws}/sending-domains/{id}", { params: { path: { ws: workspaceId, id: domain.id } } }),
        );
        if (!isAlive()) return;
        setDomain(fresh);
        if ((fresh.last_checked_at ?? null) !== before) {
          setChecking(false);
          return;
        }
      }
      setTimedOut(true);
    } catch (error) {
      if (isAlive()) setFailure(describeError(error));
    }
    if (isAlive()) setChecking(false);
  }

  const verified = domain.status === "verified";

  return (
    <>
      <div className="page-head">
        <div className="page-head-text">
          <h1 className="page-title">{domain.domain}</h1>
          <p className="page-lede">
            <Pill tone={verified ? "neutral" : "error"}>{verifiedText(domain)}</Pill>
          </p>
        </div>
        <div className="page-actions">
          <span className="muted" role="status">
            {domain.last_checked_at ? `Last checked ${formatDateTime(domain.last_checked_at)}` : "Not checked yet"}
          </span>
          <Button variant="primary" onClick={() => void recheck()} disabled={checking}>
            {checking ? "Checking DNS" : "Check DNS now"}
          </Button>
          <Button onClick={() => setDeleting(true)}>Delete domain…</Button>
        </div>
      </div>
      {failure ? <InlineError title={failure.title} detail={failure.detail} traceId={failure.traceId} /> : null}
      {timedOut ? (
        <p className="notice" role="status">
          The check has not finished yet. It runs in the background; check again in a moment.
        </p>
      ) : null}
      <p className="muted">
        Publish these {domain.records.length} TXT records at the DNS host for <span className="strong">{domain.domain}</span>. The domain is
        verified once all of them match. Until then its sender identities cannot send, and templates that use them cannot be published.
      </p>
      <ol className="rec-list" aria-label="Records to publish">
        {domain.records.map((record, at) => (
          <RecordStep key={record.kind} number={at + 1} record={record} last={at === domain.records.length - 1} />
        ))}
      </ol>
      <IdentitiesPanel workspaceId={workspaceId} domain={domain} />
      {deleting ? (
        <DeleteDomainDialog
          workspaceId={workspaceId}
          domain={domain}
          onClose={() => setDeleting(false)}
          onDeleted={() => void navigate(`/w/${workspaceId}/sending-domains`)}
        />
      ) : null}
    </>
  );
}

function IdentitiesPanel({ workspaceId, domain }: { workspaceId: string; domain: SendingDomain }) {
  const identities = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/sender-identities", {
          params: { path: { ws: workspaceId }, query: { domain_id: domain.id } },
        }),
      ),
    [workspaceId, domain.id],
  );
  const [editing, setEditing] = useState<SenderIdentity | "new" | null>(null);
  const [archiving, setArchiving] = useState<SenderIdentity | null>(null);
  const reason = blockedReason(domain);

  const add = (
    <Button onClick={() => setEditing("new")}>Add sender identity</Button>
  );

  return (
    <Panel title="Sender identities on this domain" flush>
      <ResourceView
        resource={identities}
        loadingLabel="Loading sender identities"
        isEmpty={(data) => data.length === 0}
        empty={
          <EmptyState title="No sender identities yet" action={add}>
            An identity is the name and address a message is sent from. It can send once the domain is verified.
          </EmptyState>
        }
      >
        {(data) => (
          <>
            <div className="table-scroll">
              <table className="table">
                <caption>Sender identities on {domain.domain}</caption>
                <thead>
                  <tr>
                    <th scope="col">From name</th>
                    <th scope="col">From address</th>
                    <th scope="col">Reply-to</th>
                    <th scope="col">Status</th>
                    <th scope="col">
                      <span className="visually-hidden">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((identity) => (
                    <tr key={identity.id}>
                      <td className="strong">{identity.from_name}</td>
                      <td className="mono">{identity.from_address}</td>
                      <td>{identity.reply_to ? <span className="mono">{identity.reply_to}</span> : <span className="faint">None</span>}</td>
                      <td>
                        {reason ? (
                          <span className="st st-fail">
                            <svg aria-hidden="true" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round">
                              <rect x="3.5" y="7" width="9" height="6.5" rx="1.5" />
                              <path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2" />
                            </svg>
                            <span>Blocked: {reason}</span>
                          </span>
                        ) : (
                          <span className="st st-ok">Can send</span>
                        )}
                      </td>
                      <td>
                        <div className="row">
                          <Button onClick={() => setEditing(identity)} aria-label={`Edit ${identity.from_name}, ${identity.from_address}`}>
                            Edit
                          </Button>
                          <Button onClick={() => setArchiving(identity)} aria-label={`Archive ${identity.from_name}, ${identity.from_address}`}>
                            Archive
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
        <IdentityDialog
          workspaceId={workspaceId}
          domain={domain}
          existing={editing === "new" ? undefined : editing}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            identities.reload();
          }}
        />
      ) : null}
      {archiving ? (
        <ArchiveIdentityDialog
          workspaceId={workspaceId}
          identity={archiving}
          onClose={() => setArchiving(null)}
          onArchived={() => {
            setArchiving(null);
            identities.reload();
          }}
        />
      ) : null}
    </Panel>
  );
}

function ArchiveIdentityDialog({
  workspaceId,
  identity,
  onClose,
  onArchived,
}: {
  workspaceId: string;
  identity: SenderIdentity;
  onClose: () => void;
  onArchived: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  async function archive() {
    setBusy(true);
    try {
      await unwrapEmpty(
        client.DELETE("/api/v1/workspaces/{ws}/sender-identities/{id}", { params: { path: { ws: workspaceId, id: identity.id } } }),
      );
      onArchived();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }
  return (
    <Dialog title={`Archive ${identity.from_address}?`} onClose={onClose}>
      <div className="stack">
        <p className="muted">It can no longer be chosen for a message, and its address is free again.</p>
        {failure ? <InlineError title={failure.title} detail={failure.detail} traceId={failure.traceId} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={() => void archive()} disabled={busy}>
            Archive the identity
          </Button>
        </div>
      </div>
    </Dialog>
  );
}

function DeleteDomainDialog({
  workspaceId,
  domain,
  onClose,
  onDeleted,
}: {
  workspaceId: string;
  domain: SendingDomain;
  onClose: () => void;
  onDeleted: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  async function remove() {
    setBusy(true);
    setFailure(null);
    try {
      await unwrapEmpty(
        client.DELETE("/api/v1/workspaces/{ws}/sending-domains/{id}", { params: { path: { ws: workspaceId, id: domain.id } } }),
      );
      onDeleted();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }
  return (
    <Dialog title={`Delete ${domain.domain}?`} onClose={onClose}>
      <div className="stack">
        <p className="muted">
          This deletes the domain, its records and its DKIM key. It is refused while a live sender identity is on it, or a template uses one
          of its identities.
        </p>
        {failure ? <InlineError title={failure.title} detail={failure.detail} traceId={failure.traceId} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={() => void remove()} disabled={busy}>
            Delete the domain
          </Button>
        </div>
      </div>
    </Dialog>
  );
}
