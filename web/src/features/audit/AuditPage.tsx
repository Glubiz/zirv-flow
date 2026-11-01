import { useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";

import { describeError } from "../../api";
import {
  Button,
  EmptyState,
  ErrorState,
  InlineError,
  LoadingState,
  MenuButton,
  PageHeader,
  Panel,
} from "../../ui";
import { RequireAdmin } from "../members";
import { useSignedInUser } from "../session";
import { AuditEntrySheet } from "./AuditEntrySheet";
import { AuditFilters } from "./AuditFilters";
import { AuditTable } from "./AuditTable";
import { exportUrl, filtersFromParams, paramsFromFilters, type AuditEntry, type ExportFormat, type Filters } from "./model";
import { useAuditCatalogue, useAuditEntries } from "./useAuditEntries";

/** Starts a download of `url` with the session cookie, without leaving the page. */
function download(url: string) {
  const link = document.createElement("a");
  link.href = url;
  link.download = "";
  document.body.appendChild(link);
  link.click();
  link.remove();
}

/** The audit log of the workspace, and for the instance owner the instance rows too. Admins only: anyone else is sent to the members page. */
export function AuditPage() {
  const { ws = "" } = useParams();
  return (
    <RequireAdmin workspaceId={ws}>
      <AuditContent ws={ws} />
    </RequireAdmin>
  );
}

function AuditContent({ ws }: { ws: string }) {
  const user = useSignedInUser();
  const [params, setParams] = useSearchParams();
  const instance = user.is_owner && params.get("scope") === "instance";
  const filters = filtersFromParams(params);
  const catalogue = useAuditCatalogue(ws);
  const entries = useAuditEntries(ws, instance, filters);
  const [selected, setSelected] = useState<AuditEntry | null>(null);

  function apply(next: Filters) {
    setSelected(null);
    setParams(paramsFromFilters(next, instance));
  }

  function switchScope(toInstance: boolean) {
    setSelected(null);
    setParams(paramsFromFilters(filters, toInstance));
  }

  const formats: { key: ExportFormat; label: string }[] = [
    { key: "ndjson", label: "NDJSON" },
    { key: "csv", label: "CSV" },
  ];

  return (
    <div className="page">
      <PageHeader
        title="Audit log" breadcrumb={{ group: "Compliance", section: "Audit log" }}
        lede={instance ? "Instance-level rows: sign-ins, users and keys." : "Who did what in this workspace, and when."}
        actions={
          <>
            {user.is_owner ? (
              <label className="switch-label audit-scope">
                <input
                  type="checkbox"
                  role="switch"
                  className="switch"
                  aria-label="Instance"
                  checked={instance}
                  onChange={(event) => switchScope(event.target.checked)}
                />
                <span className="audit-scope-text">Instance</span>
              </label>
            ) : null}
            <MenuButton
              label="Export"
              buttonClassName="btn btn-primary"
              buttonContent="Export"
              entries={formats.map((format) => ({
                key: format.key,
                label: format.label,
                onSelect: () => download(exportUrl(ws, instance, filters, format.key)),
              }))}
            />
          </>
        }
      />
      <p className="faint audit-note">Exporting is itself recorded in this log.</p>
      <div className="audit-layout">
        <div className="audit-main">
          <AuditFilters
            key={params.toString()}
            applied={filters}
            catalogue={catalogue}
            onApply={apply}
          />
          <Panel flush>
            {entries.resource.status === "loading" ? <LoadingState label="Loading the audit log" /> : null}
            {entries.resource.status === "error" ? (
              <ErrorState {...describeError(entries.resource.error)} onRetry={entries.resource.reload} />
            ) : null}
            {entries.resource.status === "ready" && entries.resource.data.length === 0 ? (
              <EmptyState title="No entries match these filters" />
            ) : null}
            {entries.resource.status === "ready" && entries.resource.data.length > 0 ? (
              <>
                <AuditTable entries={entries.resource.data} selectedId={selected?.id ?? null} onSelect={setSelected} />
                <div className="panel-footer">
                  {entries.moreError ? <InlineError {...describeError(entries.moreError)} /> : null}
                  <span className="faint">
                    {entries.resource.data.length} {entries.resource.data.length === 1 ? "entry" : "entries"} shown
                  </span>
                  {entries.next !== null ? (
                    <Button onClick={entries.loadMore} disabled={entries.loadingMore}>
                      {entries.loadingMore ? "Loading" : "Load more"}
                    </Button>
                  ) : null}
                </div>
              </>
            ) : null}
          </Panel>
          <p className="faint">
            How long entries are kept is set on the <Link to={`/w/${ws}/privacy`}>Privacy page</Link>.
          </p>
        </div>
        {selected ? (
          <AuditEntrySheet key={selected.id} workspaceId={ws} entry={selected} onClose={() => setSelected(null)} />
        ) : null}
      </div>
    </div>
  );
}
