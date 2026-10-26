import { useState, type ChangeEvent, type SyntheticEvent } from "react";
import { useNavigate } from "react-router";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, TextAreaField, TextField } from "../../ui";
import { asBody, startingDocument } from "./model";

interface Props {
  workspaceId: string;
  onClose: () => void;
}

/** Source that opens with an `<mjml>` tag is MJML; anything else is HTML. */
export function isMjml(source: string): boolean {
  return /^\s*(<\?xml[^>]*>\s*)?(<!--[\s\S]*?-->\s*)*<mjml[\s>]/i.test(source);
}

const MAX_SOURCE_BYTES = 1_000_000;

function nameFromFile(file: string): string {
  return file.replace(/\.[^.]+$/, "").slice(0, 100);
}

/** Makes a code-mode email template from HTML or MJML read from a file or pasted. The editor checks it as it does any source. */
export function ImportDialog({ workspaceId, onClose }: Props) {
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [source, setSource] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const mjml = isMjml(source);

  async function onFile(event: ChangeEvent<HTMLInputElement>) {
    const file = event.target.files?.[0];
    if (!file) return;
    if (file.size > MAX_SOURCE_BYTES) {
      setFailure({ title: "That file is too large", detail: "A template source can be up to 1 MB.", traceId: null });
      return;
    }
    setFailure(null);
    setSource(await file.text());
    setName((current) => (current === "" ? nameFromFile(file.name) : current));
  }

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (busy || name.trim() === "" || source.trim() === "") return;
    setBusy(true);
    setFailure(null);
    try {
      const document = startingDocument("email", mjml ? "mjml" : "html", "marketing");
      const email = { ...(document?.["email"] as Record<string, unknown>), html: source };
      const created = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/templates", {
          params: { path: { ws: workspaceId } },
          body: { name: name.trim(), channel: "email", category: "marketing", document: asBody({ ...document, email }) },
        }),
      );
      void navigate(`/w/${workspaceId}/templates/${created.id}`);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="Import HTML or MJML" onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <p className="muted">Creates a marketing email template you edit as code. Add the subject and the sender afterwards.</p>
        <div className="field">
          <label className="field-label" htmlFor="import-file">
            File
          </label>
          <input id="import-file" type="file" accept=".html,.htm,.mjml,text/html" onChange={(event) => void onFile(event)} />
          <span className="field-hint">An .html or .mjml file, or paste the source below.</span>
        </div>
        <TextAreaField
          label="Source"
          value={source}
          onChange={(event) => setSource(event.target.value)}
          rows={8}
          mono
          hint={source.trim() === "" ? undefined : mjml ? "Read as MJML." : "Read as HTML."}
        />
        <TextField
          label="Name"
          hint="Up to 100 characters. Unique among the live templates."
          value={name}
          onChange={(event) => setName(event.target.value)}
          maxLength={100}
          autoComplete="off"
          required
        />
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || name.trim() === "" || source.trim() === ""}>
            {busy ? "Importing" : "Import template"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
