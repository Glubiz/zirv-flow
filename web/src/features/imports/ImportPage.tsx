import { useEffect, useState } from "react";
import { useParams } from "react-router";

import { ApiError, client, unwrap, useResource, type Resource } from "../../api";
import { formatDateTime, PageHeader, Panel, Pill, ResourceView, Stepper } from "../../ui";
import { useImportAccess } from "./access";
import { DryRunStep } from "./DryRunStep";
import { type CsvImport, isActive, plural, statusLabel, statusTone } from "./labels";
import { MappingStep } from "./MappingStep";
import { RunStep } from "./RunStep";
import { STEPS, type Step } from "./steps";

const POLL_MS = 1000;

/** The step an import is in, from its state. */
export function stepFor(item: CsvImport): Step {
  switch (item.status) {
    case "uploaded":
      return "map";
    case "mapped":
    case "dry_running":
    case "dry_run_done":
      return "dry-run";
    case "running":
    case "succeeded":
      return "run";
    case "failed":
    case "cancelled":
      return item.started_at ? "run" : "dry-run";
  }
  return "map";
}

export function ImportPage({ pollMs = POLL_MS }: { pollMs?: number }) {
  const { ws = "", importId = "" } = useParams();
  return <ImportView key={`${ws}/${importId}`} workspaceId={ws} importId={importId} pollMs={pollMs} />;
}

function ImportView({ workspaceId, importId, pollMs }: { workspaceId: string; importId: string; pollMs: number }) {
  const { canWrite, canPublish } = useImportAccess(workspaceId);
  const polled = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/imports/{id}", {
          params: { path: { ws: workspaceId, id: importId } },
        }),
      ),
    [workspaceId, importId],
  );
  const [view, setView] = useState<Step | null>(null);

  // A failed poll must not blank a page the user is reading: keep the last good import and keep polling.
  // Only a first load that fails, or an import that is gone (404), shows the error.
  const [held, setHeld] = useState<CsvImport | null>(null);
  const gone = polled.status === "error" && polled.error instanceof ApiError && polled.error.status === 404;
  if (polled.status === "ready" && held !== polled.data) {
    setHeld(polled.data);
  } else if (gone && held !== null) {
    setHeld(null);
  }
  const resource: Resource<CsvImport> =
    polled.status !== "ready" && held !== null && !gone ? { status: "ready", data: held, reload: polled.reload, refreshing: true } : polled;

  const active = resource.status === "ready" && isActive(resource.data.status);
  const reload = resource.reload;
  useEffect(() => {
    if (!active) return;
    const timer = setInterval(reload, pollMs);
    return () => clearInterval(timer);
  }, [active, reload, pollMs]);

  return (
    <div className="page">
      <ResourceView resource={resource} loadingLabel="Loading the import">
        {(item) => {
          const step = view ?? stepFor(item);
          const changed = () => {
            setView(null);
            resource.reload();
          };
          return (
            <>
              <PageHeader
                title={item.name}
                breadcrumb={{ group: "Data", section: "Imports", to: `/w/${workspaceId}/imports` }}
                lede={`${item.file_name}: ${plural(item.rows_total, "row", "rows")}, ${plural(item.header.length, "column", "columns")}.`}
                actions={<Pill tone={statusTone(item.status)}>{statusLabel(item.status)}</Pill>}
              />
              <Stepper steps={STEPS} current={step} label="Import steps" />
              <Panel>
                {step === "map" ? (
                  <MappingStep
                    workspaceId={workspaceId}
                    importId={item.id}
                    canWrite={canWrite && !isActive(item.status)}
                    onSaved={changed}
                  />
                ) : null}
                {step === "dry-run" ? (
                  <DryRunStep
                    workspaceId={workspaceId}
                    item={item}
                    canWrite={canWrite}
                    canPublish={canPublish}
                    onChanged={changed}
                    onEditMapping={() => setView("map")}
                  />
                ) : null}
                {step === "run" ? <RunStep workspaceId={workspaceId} item={item} canWrite={canWrite} onChanged={changed} /> : null}
              </Panel>
              <p className="faint">
                Uploaded {formatDateTime(item.created_at)}.
              </p>
            </>
          );
        }}
      </ResourceView>
    </div>
  );
}
