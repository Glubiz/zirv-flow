import { client, unwrap } from "../../api";
import { Button, InlineError } from "../../ui";
import { errorText, num, plural, rejectsUrl, type CsvImport } from "./labels";
import { ProgressBar } from "./ProgressBar";
import { RejectsTable } from "./RejectsTable";
import { useAction } from "./useAction";

interface DryRunStepProps {
  workspaceId: string;
  item: CsvImport;
  canWrite: boolean;
  canPublish: boolean;
  /** The import changed on the server; load it again. */
  onChanged: () => void;
  onEditMapping: () => void;
}

/** More than this share of rows rejected would stop a run once it is past 1,000 rows. */
function overThreshold(item: CsvImport): boolean {
  return item.rows_total > 1000 && item.rows_rejected / Math.max(item.rows_done, 1) > item.reject_threshold;
}

export function DryRunStep({ workspaceId, item, canWrite, canPublish, onChanged, onEditMapping }: DryRunStepProps) {
  const action = useAction();
  const path = { ws: workspaceId, id: item.id };

  async function start(kind: "dry-run" | "run") {
    const done = await action.perform(() =>
      unwrap(
        kind === "dry-run"
          ? client.POST("/api/v1/workspaces/{ws}/imports/{id}/dry-run", { params: { path } })
          : client.POST("/api/v1/workspaces/{ws}/imports/{id}/run", { params: { path } }),
      ),
    );
    if (done) onChanged();
  }

  async function cancel() {
    const done = await action.perform(() =>
      unwrap(client.POST("/api/v1/workspaces/{ws}/imports/{id}/cancel", { params: { path } })),
    );
    if (done) onChanged();
  }

  const failed = item.status === "failed" || item.status === "cancelled";

  return (
    <div className="stack">
      {item.status === "mapped" ? (
        <>
          <p>
            The mapping is saved. A dry run reads the whole file with the mapping and shows which rows would be rejected. It
            writes no people, fields or lists.
          </p>
          {canWrite ? (
            <div className="row">
              <Button variant="primary" onClick={() => void start("dry-run")} disabled={action.busy}>
                Run the dry run
              </Button>
              {canPublish ? (
                <Button onClick={() => void start("run")} disabled={action.busy}>
                  Skip it and start the import
                </Button>
              ) : null}
              <Button onClick={onEditMapping} disabled={action.busy}>
                Edit the mapping
              </Button>
            </div>
          ) : null}
        </>
      ) : null}
      {item.status === "dry_running" ? (
        <>
          <p role="status">
            Checking row {num(item.rows_done)} of {num(item.rows_total)}.
          </p>
          <ProgressBar label="Dry run progress" done={item.rows_done} total={item.rows_total} />
          {canWrite ? (
            <div className="row">
              <Button onClick={() => void cancel()} disabled={action.busy}>
                Cancel the dry run
              </Button>
            </div>
          ) : null}
        </>
      ) : null}
      {item.status === "dry_run_done" ? (
        <>
          <div role="status" className="stack-tight">
            <p className="strong">
              {plural(item.rows_total, "row", "rows")} checked: {num(item.rows_applied)} would be imported
              {item.rows_rejected > 0 ? `, ${num(item.rows_rejected)} would be rejected` : ""}.
            </p>
            {overThreshold(item) ? (
              <p className="attention">
                More than {Math.round(item.reject_threshold * 100)}% of the rows are rejected. A run stops when that happens
                after the first 1,000 rows; rows applied before then stay applied. Fix the mapping or the file first.
              </p>
            ) : null}
          </div>
          {item.rows_rejected > 0 ? (
            <>
              <RejectsTable workspaceId={workspaceId} importId={item.id} phase="dry_run" total={item.rows_rejected} />
              <p>
                <a href={rejectsUrl(workspaceId, item.id, "dry_run")} download>
                  Download all rejected rows
                </a>
              </p>
            </>
          ) : null}
          {canWrite ? (
            <div className="row">
              {canPublish ? (
                <Button variant="primary" onClick={() => void start("run")} disabled={action.busy}>
                  {action.busy ? "Starting" : "Start the import"}
                </Button>
              ) : null}
              <Button onClick={onEditMapping} disabled={action.busy}>
                Back to the mapping
              </Button>
              <Button onClick={() => void start("dry-run")} disabled={action.busy}>
                Run the dry run again
              </Button>
            </div>
          ) : null}
        </>
      ) : null}
      {failed ? (
        <div className="stack-tight">
          <p className="strong" role="status">
            {item.status === "cancelled" ? "The dry run was cancelled." : "The dry run failed."}
          </p>
          {item.status === "failed" ? <p>{errorText(item.error_code)}</p> : null}
        </div>
      ) : null}
      {action.failure ? <InlineError {...action.failure} /> : null}
    </div>
  );
}
