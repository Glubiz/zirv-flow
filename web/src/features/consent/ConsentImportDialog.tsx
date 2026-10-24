import { useState, type ChangeEvent, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, TextAreaField } from "../../ui";
import type { Dimension } from "./DimensionDialog";
import { importProblemLabel, plural } from "./labels";

type Outcome = components["schemas"]["ConsentImportBody"];

interface ConsentImportDialogProps {
  workspaceId: string;
  /** The workspace dimensions; each adds a `dimension:<key>` column to the file. */
  dimensions: readonly Dimension[];
  onClose: () => void;
  onImported: (message: string) => void;
}

/** Imports consent states from a CSV, with a preview first. A dimension column scopes a row to one value. */
export function ConsentImportDialog({ workspaceId, dimensions, onClose, onImported }: ConsentImportDialogProps) {
  const [text, setText] = useState("");
  const [preview, setPreview] = useState<{ text: string; outcome: Outcome } | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  // The columns of a consent file: the fixed four, then one per dimension the workspace declared and has not archived.
  const declared = dimensions.filter((dimension) => !dimension.builtin && !dimension.archived);
  const header = ["person_id", "channel", "topic", "status", ...declared.map((dimension) => `dimension:${dimension.key}`)].join(",");

  const current = preview !== null && preview.text === text ? preview.outcome : null;

  async function send(dryRun: boolean): Promise<Outcome> {
    return unwrap(
      client.POST("/api/v1/workspaces/{ws}/consent/import", {
        params: { path: { ws: workspaceId }, query: { dry_run: dryRun } },
        body: text,
        bodySerializer: (body) => body,
        headers: { "content-type": "text/csv" },
      }),
    );
  }

  async function check(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      setPreview({ text, outcome: await send(true) });
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  async function commit() {
    setBusy(true);
    setFailure(null);
    try {
      const outcome = await send(false);
      onImported(
        `Imported: ${plural(outcome.changed, "state changed", "states changed")}, ${outcome.unchanged} unchanged, ${plural(outcome.invalid.length, "row skipped", "rows skipped")}.`,
      );
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  async function pickFile(event: ChangeEvent<HTMLInputElement>) {
    const file = event.target.files?.[0];
    if (file) {
      setText(await file.text());
    }
  }

  return (
    <Dialog title="Import consent" onClose={onClose} wide>
      <form className="stack" onSubmit={(event) => void check(event)} noValidate>
        <p className="muted">
          One row per choice, with the header <span className="mono">{header}</span>. The status is subscribed, unsubscribed or
          pending_confirmation. Leave a dimension column empty to cover all of its values. Up to 10,000 rows and 10 MiB.
        </p>
        {declared.length > 0 ? (
          <p className="muted">
            Dimension columns in this workspace:{" "}
            {declared.map((dimension, at) => (
              <span key={dimension.key}>
                {at > 0 ? ", " : ""}
                <span className="mono">dimension:{dimension.key}</span> ({dimension.values.filter((value) => !value.archived).map((value) => value.key).join(", ") || "no values yet"})
              </span>
            ))}
            .
          </p>
        ) : null}
        <div className="field">
          <label className="field-label" htmlFor="consent-file">
            CSV file
          </label>
          <input id="consent-file" type="file" accept=".csv,text/csv" onChange={(event) => void pickFile(event)} />
        </div>
        <TextAreaField
          label="Or paste the CSV"
          mono
          placeholder={`${header}\n`}
          value={text}
          onChange={(event) => setText(event.target.value)}
          spellCheck={false}
        />
        {current ? (
          <div className="notice" role="status">
            <p className="strong">
              {plural(current.changed, "state", "states")} would change, {current.unchanged} already hold that choice.
            </p>
            {current.invalid.length > 0 ? (
              <>
                <p>{plural(current.invalid.length, "row", "rows")} will be skipped:</p>
                <ul className="list-plain">
                  {current.invalid.slice(0, 20).map((row) => (
                    <li key={row.row}>
                      Line {row.row}: {importProblemLabel(row.reason)}
                    </li>
                  ))}
                </ul>
                {current.invalid.length > 20 ? <p className="faint">And {current.invalid.length - 20} more.</p> : null}
              </>
            ) : null}
          </div>
        ) : null}
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" disabled={busy || text.trim() === ""}>
            {busy && !current ? "Checking" : "Preview"}
          </Button>
          <Button variant="primary" onClick={() => void commit()} disabled={busy || current === null || current.changed === 0}>
            {busy && current ? "Importing" : "Import"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
