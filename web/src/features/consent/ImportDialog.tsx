import { useState, type ChangeEvent, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, TextAreaField } from "../../ui";
import { rowProblemLabel } from "./labels";

type Outcome = components["schemas"]["SuppressionImportBody"];

interface ImportDialogProps {
  workspaceId: string;
  onClose: () => void;
  onImported: (message: string) => void;
}

const SAMPLE = "kind,value,note\nemail,person@example.com,Asked to stop by phone\n";

function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

export function ImportDialog({ workspaceId, onClose, onImported }: ImportDialogProps) {
  const [text, setText] = useState("");
  const [preview, setPreview] = useState<{ text: string; outcome: Outcome } | null>(null);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const current = preview !== null && preview.text === text ? preview.outcome : null;

  async function send(dryRun: boolean): Promise<Outcome> {
    return unwrap(
      client.POST("/api/v1/workspaces/{ws}/suppressions/import", {
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
        `Imported: ${plural(outcome.added, "address suppressed", "addresses suppressed")}, ${outcome.existing} already suppressed, ${plural(outcome.invalid.length, "row skipped", "rows skipped")}.`,
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
    <Dialog title="Import suppressions" onClose={onClose} wide>
      <form className="stack" onSubmit={(event) => void check(event)} noValidate>
        <p className="muted">
          One row per address or number, with the header <span className="mono">kind,value</span> and an optional{" "}
          <span className="mono">note</span> column. Kinds are <span className="mono">email</span> and{" "}
          <span className="mono">phone</span>. Up to 100,000 rows and 10 MiB. Values are hashed on import and not kept.
        </p>
        <div className="field">
          <label className="field-label" htmlFor="suppression-file">
            CSV file
          </label>
          <input id="suppression-file" type="file" accept=".csv,text/csv" onChange={(event) => void pickFile(event)} />
        </div>
        <TextAreaField
          label="Or paste the CSV"
          mono
          placeholder={SAMPLE}
          value={text}
          onChange={(event) => setText(event.target.value)}
          spellCheck={false}
        />
        {current ? (
          <div className="notice" role="status">
            <p className="strong">
              {plural(current.added, "address", "addresses")} would be suppressed, {current.existing} already suppressed.
            </p>
            {current.invalid.length > 0 ? (
              <>
                <p>{plural(current.invalid.length, "row", "rows")} will be skipped:</p>
                <ul className="list-plain">
                  {current.invalid.slice(0, 20).map((row) => (
                    <li key={row.row}>
                      Line {row.row}: {rowProblemLabel(row.reason)}
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
          <Button variant="primary" onClick={() => void commit()} disabled={busy || current === null || current.added === 0}>
            {busy && current ? "Importing" : "Import"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
