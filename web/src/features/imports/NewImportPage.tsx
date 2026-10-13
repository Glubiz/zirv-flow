import { useState, type ChangeEvent, type DragEvent } from "react";
import { Link, useNavigate, useParams } from "react-router";

import { describeError, uploadFile, type components, type ErrorSummary } from "../../api";
import { Button, InlineError, PageHeader, Panel, SelectField, Stepper, TextField } from "../../ui";
import { useImportAccess } from "./access";
import { percent } from "./labels";
import { STEPS } from "./steps";

type Created = components["schemas"]["CsvImportDetailBody"];

const MAX_NAME_CHARS = 128;

export function NewImportPage() {
  const { ws = "" } = useParams();
  return <NewImport key={ws} workspaceId={ws} />;
}

function sizeText(bytes: number): string {
  if (bytes < 1024) return `${bytes} bytes`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

function nameFrom(fileName: string): string {
  const stem = fileName.includes(".") ? fileName.slice(0, fileName.lastIndexOf(".")) : fileName;
  return stem.slice(0, MAX_NAME_CHARS);
}

export function uploadUrl(workspaceId: string, params: { name: string; fileName: string; stream: string; delimiter: string }): string {
  const query = new URLSearchParams({ name: params.name, file_name: params.fileName });
  if (params.stream.trim() !== "") query.set("stream", params.stream.trim());
  if (params.delimiter !== "") query.set("delimiter", params.delimiter);
  return `/api/v1/workspaces/${workspaceId}/imports?${query.toString()}`;
}

function NewImport({ workspaceId }: { workspaceId: string }) {
  const { canWrite, ready } = useImportAccess(workspaceId);
  const navigate = useNavigate();
  const [file, setFile] = useState<File | null>(null);
  const [name, setName] = useState("");
  const [stream, setStream] = useState("");
  const [delimiter, setDelimiter] = useState("");
  const [dragging, setDragging] = useState(false);
  const [sent, setSent] = useState<{ done: number; total: number } | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  function choose(chosen: File | undefined) {
    setFailure(null);
    if (!chosen) return;
    setFile(chosen);
    setName(nameFrom(chosen.name));
  }

  function onPick(event: ChangeEvent<HTMLInputElement>) {
    choose(event.target.files?.[0]);
  }

  function onDrop(event: DragEvent<HTMLDivElement>) {
    event.preventDefault();
    setDragging(false);
    choose(event.dataTransfer.files[0]);
  }

  async function upload() {
    if (!file) return;
    setFailure(null);
    setSent({ done: 0, total: file.size });
    try {
      const created = await uploadFile<Created>(
        uploadUrl(workspaceId, { name: name.trim(), fileName: file.name, stream, delimiter }),
        file,
        "text/csv",
        (done, total) => setSent({ done, total }),
      );
      void navigate(`/w/${workspaceId}/imports/${created.id}`);
    } catch (error) {
      setFailure(describeError(error));
      setSent(null);
    }
  }

  const uploading = sent !== null;
  const nameOk = name.trim() !== "" && name.trim().length <= MAX_NAME_CHARS;

  return (
    <div className="page">
      <PageHeader
        title="New import"
        lede="Upload a CSV file of people. The first row names the columns. You choose what each column becomes in the next step; nothing is written until you run the import."
      />
      <Stepper steps={STEPS} current="upload" label="Import steps" />
      <Panel>
        {!ready ? null : !canWrite ? (
          <p className="muted">
            You can look at imports but not start one. <Link to={`/w/${workspaceId}/imports`}>Back to imports</Link>
          </p>
        ) : (
          <form
            className="stack"
            onSubmit={(event) => {
              event.preventDefault();
              void upload();
            }}
          >
            <div
              className={dragging ? "dropzone dropzone-over" : "dropzone"}
              onDragOver={(event) => {
                event.preventDefault();
                setDragging(true);
              }}
              onDragLeave={() => setDragging(false)}
              onDrop={onDrop}
            >
              <p className="strong">{file ? file.name : "Drag a CSV file here"}</p>
              <p className="muted">
                {file ? sizeText(file.size) : "or choose one from your computer. UTF-8 text with a header row; comma, semicolon or tab separated."}
              </p>
              <div className="field">
                <label className="field-label" htmlFor="import-file">
                  CSV file
                </label>
                <input
                  id="import-file"
                  type="file"
                  accept=".csv,.tsv,.txt,text/csv,text/plain"
                  onChange={onPick}
                  disabled={uploading}
                />
              </div>
            </div>
            <TextField
              label="Name"
              value={name}
              onChange={(event) => setName(event.target.value)}
              maxLength={MAX_NAME_CHARS}
              disabled={uploading}
              hint="Shown in the import history."
            />
            <details className="advanced">
              <summary>Advanced</summary>
              <div className="stack">
                <TextField
                  label="Stream"
                  value={stream}
                  onChange={(event) => setStream(event.target.value)}
                  placeholder="Taken from the file name"
                  disabled={uploading}
                  mono
                  hint="Records are stored under the source csv and this stream. Use the same stream each time you import the same data, and a run changes only what changed."
                />
                <SelectField
                  label="Delimiter"
                  value={delimiter}
                  onChange={(event) => setDelimiter(event.target.value)}
                  disabled={uploading}
                >
                  <option value="">Detect automatically</option>
                  <option value="comma">Comma</option>
                  <option value="semicolon">Semicolon</option>
                  <option value="tab">Tab</option>
                </SelectField>
              </div>
            </details>
            {sent ? (
              <div role="status" className="stack-tight">
                <progress
                  className="progress"
                  max={Math.max(sent.total, 1)}
                  value={sent.done}
                  aria-label="Upload progress"
                />
                <span className="muted">
                  {sent.done >= sent.total && sent.total > 0
                    ? "Uploaded. Checking the file."
                    : `Uploading ${percent(sent.done, sent.total)}%`}
                </span>
              </div>
            ) : null}
            {failure ? <InlineError {...failure} /> : null}
            <div className="row">
              <Button variant="primary" type="submit" disabled={!file || !nameOk || uploading}>
                {uploading ? "Uploading" : "Upload and continue"}
              </Button>
              <Link to={`/w/${workspaceId}/imports`}>Cancel</Link>
            </div>
          </form>
        )}
      </Panel>
    </div>
  );
}
