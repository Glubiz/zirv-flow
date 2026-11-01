import { useState } from "react";
import { useParams } from "react-router";

import { describeError } from "../../api";
import {
  Button,
  EmptyState,
  formatDateTime,
  InlineError,
  kindLabel,
  PageHeader,
  Panel,
  Pill,
  ResourceView,
  SelectField,
} from "../../ui";
import { useWorkspaceAccess } from "../members";
import { ConflictDialog } from "./ConflictDialog";
import { reasonLabel, resolutionLabel } from "./conflictLabels";
import { useConflicts, type Conflict, type ConflictStatus } from "./useConflicts";

export function ConflictsPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);
  const canManage = access.status === "ready" && access.data.canAdmin;

  return (
    <div className="page">
      <PageHeader
        title="Identity conflicts" breadcrumb={{ group: "Audience", section: "People" }}
        lede="Places where two people share an identifier but could not be merged. Decide whether they stay separate or the identifier moves."
      />
      <ConflictList key={ws} workspaceId={ws} canManage={canManage} />
    </div>
  );
}

function ConflictList({ workspaceId, canManage }: { workspaceId: string; canManage: boolean }) {
  const [status, setStatus] = useState<ConflictStatus>("open");
  const conflicts = useConflicts(workspaceId, status);
  const [reviewing, setReviewing] = useState<Conflict | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  return (
    <>
      <div className="page-actions">
        <SelectField
          label="Show"
          value={status}
          onChange={(event) => {
            setNotice(null);
            setStatus(event.target.value === "resolved" ? "resolved" : "open");
          }}
        >
          <option value="open">Open conflicts</option>
          <option value="resolved">Resolved conflicts</option>
        </SelectField>
      </div>
      {notice ? (
        <p className="notice" role="status">
          {notice}
        </p>
      ) : null}
      <Panel flush>
        <ResourceView
          resource={conflicts.resource}
          loadingLabel="Loading conflicts"
          isEmpty={(data) => data.length === 0}
          empty={
            status === "open" ? (
              <EmptyState title="No open conflicts">
                When a source sends an identifier that two people share and they cannot be merged, it shows up here for review.
              </EmptyState>
            ) : (
              <EmptyState title="No resolved conflicts yet">Conflicts you resolve, and those closed by a merge, are listed here.</EmptyState>
            )
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>{status === "open" ? "Open identity conflicts" : "Resolved identity conflicts"}</caption>
                <thead>
                  <tr>
                    <th scope="col">Shared identifier</th>
                    <th scope="col">Why they were not merged</th>
                    <th scope="col">Found</th>
                    <th scope="col">{status === "open" ? "Status" : "Outcome"}</th>
                    <th scope="col">
                      <span className="visually-hidden">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((conflict) => (
                    <tr key={conflict.id}>
                      <td className="strong">{kindLabel(conflict.kind)}</td>
                      <td>
                        <Pill tone="attention">{reasonLabel(conflict.reason)}</Pill>
                      </td>
                      <td>{formatDateTime(conflict.created_at)}</td>
                      <td>{conflict.resolved_at ? resolutionLabel(conflict.resolution) : "Open"}</td>
                      <td className="num">
                        <Button
                          onClick={() => setReviewing(conflict)}
                          aria-label={`${conflict.resolved_at ? "View" : "Review"} ${kindLabel(conflict.kind)} conflict from ${formatDateTime(conflict.created_at)}`}
                        >
                          {conflict.resolved_at ? "View" : "Review"}
                        </Button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
        {conflicts.resource.status === "ready" && conflicts.nextBefore !== null ? (
          <div className="panel-footer">
            {conflicts.moreError ? <InlineError {...describeError(conflicts.moreError)} /> : null}
            <Button onClick={conflicts.loadMore} disabled={conflicts.loadingMore}>
              {conflicts.loadingMore ? "Loading" : "Show more"}
            </Button>
          </div>
        ) : null}
      </Panel>
      {!canManage ? (
        <p className="muted">Only workspace admins and the instance owner can resolve conflicts. Identifier values are hidden from you.</p>
      ) : null}
      {reviewing ? (
        <ConflictDialog
          workspaceId={workspaceId}
          conflict={reviewing}
          canManage={canManage}
          onClose={() => setReviewing(null)}
          onResolved={(message) => {
            setReviewing(null);
            setNotice(message);
            conflicts.resource.reload();
          }}
        />
      ) : null}
    </>
  );
}
