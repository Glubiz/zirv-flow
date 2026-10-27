import { useState } from "react";
import { useParams } from "react-router";

import { describeError } from "../../api";
import { Button, EmptyState, formatDateTime, InlineError, PageHeader, Panel, Pill, ResourceView, SelectField } from "../../ui";
import { useWorkspaceAccess } from "../members";
import { AddSuppressionDialog } from "./AddSuppressionDialog";
import { ConsentNav } from "./ConsentNav";
import { CheckPanel } from "./CheckPanel";
import { ImportDialog } from "./ImportDialog";
import { kindLabel, reasonLabel, REASONS } from "./labels";
import { useSuppressions } from "./useSuppressions";

export function SuppressionsPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);
  const canManage = access.status === "ready" && access.data.canAdmin;

  return (
    <div className="page">
      <ConsentNav workspaceId={ws} />
      <PageHeader
        title="Suppressions"
        lede="Addresses and numbers that never receive a message, whatever a person has chosen: bounces, complaints, manual blocks and erased people. Only a one-way hash of each is kept."
      />
      <SuppressionList key={ws} workspaceId={ws} canManage={canManage} />
    </div>
  );
}

function SuppressionList({ workspaceId, canManage }: { workspaceId: string; canManage: boolean }) {
  const [reason, setReason] = useState("");
  const suppressions = useSuppressions(workspaceId, reason);
  const [adding, setAdding] = useState(false);
  const [importing, setImporting] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  function done(message: string) {
    setAdding(false);
    setImporting(false);
    setNotice(message);
    suppressions.resource.reload();
  }

  return (
    <>
      {canManage ? (
        <div className="page-actions">
          <Button variant="primary" onClick={() => setAdding(true)}>
            Suppress an address
          </Button>
          <Button onClick={() => setImporting(true)}>Import CSV</Button>
          <a className="btn" href={`/api/v1/workspaces/${workspaceId}/suppressions/export`} download="suppressions.csv">
            Export CSV
          </a>
        </div>
      ) : null}
      {notice ? (
        <p className="notice" role="status">
          {notice}
        </p>
      ) : null}
      <CheckPanel workspaceId={workspaceId} canManage={canManage} onRemoved={done} />
      <div className="page-actions">
        <SelectField label="Show" value={reason} onChange={(event) => setReason(event.target.value)}>
          <option value="">All reasons</option>
          {REASONS.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </SelectField>
      </div>
      <Panel flush>
        <ResourceView
          resource={suppressions.resource}
          loadingLabel="Loading suppressions"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState title={reason === "" ? "Nothing is suppressed" : "Nothing suppressed for this reason"}>
              {canManage
                ? "Suppress an address, or import a list, to stop messages to it everywhere in this workspace."
                : "Workspace admins can suppress addresses."}
            </EmptyState>
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Suppressed addresses and numbers</caption>
                <thead>
                  <tr>
                    <th scope="col">Identifier</th>
                    <th scope="col">Kind</th>
                    <th scope="col">Reason</th>
                    <th scope="col">Source</th>
                    <th scope="col">Added</th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((entry, index) => (
                    <tr key={`${entry.hash_prefix}-${entry.created_at}-${index}`}>
                      <td className="mono">
                        {entry.hash_prefix}…
                        {entry.note ? <div className="faint">{entry.note}</div> : null}
                      </td>
                      <td>{kindLabel(entry.kind)}</td>
                      <td>
                        <Pill tone={entry.reason === "erased" ? "error" : entry.reason === "manual" ? "neutral" : "attention"}>
                          {reasonLabel(entry.reason)}
                        </Pill>
                      </td>
                      <td>{entry.source}</td>
                      <td>{formatDateTime(entry.created_at)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
        {suppressions.resource.status === "ready" && suppressions.nextBefore !== null ? (
          <div className="panel-footer">
            {suppressions.moreError ? <InlineError {...describeError(suppressions.moreError)} /> : null}
            <Button onClick={suppressions.loadMore} disabled={suppressions.loadingMore}>
              {suppressions.loadingMore ? "Loading" : "Show more"}
            </Button>
          </div>
        ) : null}
      </Panel>
      {!canManage ? (
        <p className="muted">Only workspace admins and the instance owner can add, import, export or remove suppressions.</p>
      ) : null}
      {adding ? <AddSuppressionDialog workspaceId={workspaceId} onClose={() => setAdding(false)} onAdded={done} /> : null}
      {importing ? <ImportDialog workspaceId={workspaceId} onClose={() => setImporting(false)} onImported={done} /> : null}
    </>
  );
}
