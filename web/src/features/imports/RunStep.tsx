import { Link } from "react-router";

import { client, unwrap } from "../../api";
import { Button, formatDateTime, InlineError } from "../../ui";
import { errorText, num, plural, rejectsUrl, type CsvImport } from "./labels";
import { ProgressBar } from "./ProgressBar";
import { RejectsTable } from "./RejectsTable";
import { useAction } from "./useAction";

interface RunStepProps {
  workspaceId: string;
  item: CsvImport;
  canWrite: boolean;
  onChanged: () => void;
}

function Counts({ item }: { item: CsvImport }) {
  return (
    <dl className="facts">
      <div className="fact">
        <dt>Rows read</dt>
        <dd>
          {num(item.rows_done)} of {num(item.rows_total)}
        </dd>
      </div>
      <div className="fact">
        <dt>Applied</dt>
        <dd>{num(item.rows_applied)}</dd>
      </div>
      <div className="fact">
        <dt>Unchanged</dt>
        <dd>{num(item.rows_unchanged)}</dd>
      </div>
      <div className="fact">
        <dt>Rejected</dt>
        <dd>{num(item.rows_rejected)}</dd>
      </div>
    </dl>
  );
}

export function RunStep({ workspaceId, item, canWrite, onChanged }: RunStepProps) {
  const action = useAction();

  async function cancel() {
    const done = await action.perform(() =>
      unwrap(
        client.POST("/api/v1/workspaces/{ws}/imports/{id}/cancel", { params: { path: { ws: workspaceId, id: item.id } } }),
      ),
    );
    if (done) onChanged();
  }

  const ended = item.status === "succeeded" || item.status === "failed" || item.status === "cancelled";

  return (
    <div className="stack">
      {item.status === "running" ? (
        <>
          <p role="status">
            Importing row {num(item.rows_done)} of {num(item.rows_total)}. You can leave this page; the
            import carries on and picks up where it stopped if it is interrupted.
          </p>
          <ProgressBar label="Import progress" done={item.rows_done} total={item.rows_total} />
        </>
      ) : null}
      {item.status === "succeeded" ? (
        <p className="strong" role="status">
          Imported {plural(item.rows_total, "row", "rows")}
          {item.finished_at ? ` on ${formatDateTime(item.finished_at)}` : ""}.
        </p>
      ) : null}
      {item.status === "failed" ? (
        <div className="stack-tight" role="alert">
          <p className="strong">The import failed.</p>
          <p>{errorText(item.error_code)}</p>
        </div>
      ) : null}
      {item.status === "cancelled" ? (
        <p className="strong" role="status">
          The import was cancelled after {plural(item.rows_done, "row", "rows")}. Rows already applied stay applied.
        </p>
      ) : null}
      {item.rows_done > 0 || ended ? <Counts item={item} /> : null}
      {ended && item.rows_rejected > 0 ? (
        <>
          <RejectsTable workspaceId={workspaceId} importId={item.id} phase="run" total={item.rows_rejected} />
          <p>
            <a href={rejectsUrl(workspaceId, item.id, "run")} download>
              Download all rejected rows
            </a>
          </p>
        </>
      ) : null}
      {item.status === "running" && canWrite ? (
        <div className="row">
          <Button onClick={() => void cancel()} disabled={action.busy}>
            Cancel the import
          </Button>
        </div>
      ) : null}
      {action.failure ? <InlineError {...action.failure} /> : null}
      {ended ? (
        <p>
          <Link to={`/w/${workspaceId}/imports`}>Back to imports</Link>
        </p>
      ) : null}
    </div>
  );
}
