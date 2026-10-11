import { useState, type ChangeEvent } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError } from "../../ui";
import { plural } from "./access";

type ImportResult = components["schemas"]["ImportBody"];

const SHOWN_ROWS = 20;

export function reasonText(reason: string): string {
  switch (reason) {
    case "not_found":
      return "no person has this identifier";
    case "malformed_row":
      return "the row does not have exactly a kind and a value";
    case "unknown_identifier_kind":
      return "the kind is not email, phone, anonymous_id or external:<name>";
    case "invalid_identifier_namespace":
      return "the external name is not valid";
    case "invalid_email":
      return "not a valid email address";
    case "invalid_phone":
      return "not a valid phone number";
    case "invalid_identifier_value":
      return "not a valid value";
    default:
      return reason;
  }
}

interface ImportDialogProps {
  workspaceId: string;
  listId: string;
  onClose: () => void;
  onImported: (result: ImportResult) => void;
}

export function ImportDialog({ workspaceId, listId, onClose, onImported }: ImportDialogProps) {
  const [fileName, setFileName] = useState<string | null>(null);
  const [text, setText] = useState<string | null>(null);
  const [preview, setPreview] = useState<ImportResult | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function choose(event: ChangeEvent<HTMLInputElement>) {
    const file = event.target.files?.[0];
    setPreview(null);
    setFailure(null);
    if (!file) {
      setFileName(null);
      setText(null);
      return;
    }
    setFileName(file.name);
    try {
      setText(await file.text());
    } catch (error) {
      setText(null);
      setFailure(describeError(error));
    }
  }

  async function send(dryRun: boolean): Promise<ImportResult | null> {
    if (text === null) return null;
    setBusy(true);
    setFailure(null);
    try {
      return await unwrap(
        client.POST("/api/v1/workspaces/{ws}/lists/{id}/members/import", {
          params: { path: { ws: workspaceId, id: listId }, query: { dry_run: dryRun } },
          body: text,
          bodySerializer: (body) => body,
          headers: { "Content-Type": "text/csv" },
        }),
      );
    } catch (error) {
      setFailure(describeError(error));
      return null;
    } finally {
      setBusy(false);
    }
  }

  async function check() {
    const result = await send(true);
    if (result) setPreview(result);
  }

  async function commit() {
    const result = await send(false);
    if (result) onImported(result);
  }

  return (
    <Dialog title="Import people from a CSV" onClose={onClose} wide>
      <div className="stack">
        <p className="muted">
          The file needs a header row <span className="mono">kind,value</span>, then one identifier per row, such as{" "}
          <span className="mono">email,ada@example.com</span>. Only people who already exist are added; no one is created.
        </p>
        <div className="field">
          <label className="field-label" htmlFor="import-file">
            CSV file
          </label>
          <input id="import-file" type="file" accept=".csv,text/csv" onChange={(event) => void choose(event)} />
          <span className="field-hint">Up to 10 MiB and 100,000 rows.</span>
        </div>
        {preview ? (
          <div role="status" className="stack-tight">
            <p className="strong">
              {fileName}: {plural(preview.matched, "row", "rows")} matched a person.
            </p>
            <p>
              {plural(preview.added, "person", "people")} would be added.{" "}
              {plural(preview.already_member, "row", "rows")} would change nothing because the person is already in the list
              or appears earlier in the file.
            </p>
            {preview.unmatched_count > 0 ? (
              <>
                <p className="strong">
                  {plural(preview.unmatched_count, "row", "rows")} will be skipped:
                </p>
                <ul className="list-plain">
                  {preview.unmatched.slice(0, SHOWN_ROWS).map((row) => (
                    <li key={row.row}>
                      Row {row.row}: {reasonText(row.reason)}
                    </li>
                  ))}
                </ul>
                {preview.unmatched_count > SHOWN_ROWS ? (
                  <p className="faint">and {preview.unmatched_count - SHOWN_ROWS} more.</p>
                ) : null}
              </>
            ) : null}
          </div>
        ) : null}
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          {preview ? (
            <Button variant="primary" onClick={() => void commit()} disabled={busy || preview.matched === 0}>
              {busy ? "Adding" : `Add ${plural(preview.added, "person", "people")}`}
            </Button>
          ) : (
            <Button variant="primary" onClick={() => void check()} disabled={busy || text === null}>
              {busy ? "Checking" : "Check file"}
            </Button>
          )}
        </div>
      </div>
    </Dialog>
  );
}
