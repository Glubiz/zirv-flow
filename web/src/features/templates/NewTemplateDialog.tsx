import { useState, type SyntheticEvent } from "react";
import { useNavigate } from "react-router";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, SelectField, TextField } from "../../ui";
import { asBody, MODES, startingDocument, type EditMode } from "./model";

interface Props {
  workspaceId: string;
  onClose: () => void;
}

export function NewTemplateDialog({ workspaceId, onClose }: Props) {
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [channel, setChannel] = useState<"email" | "webhook">("email");
  const [category, setCategory] = useState<"marketing" | "transactional">("marketing");
  const [mode, setMode] = useState<EditMode>("html");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (busy || name.trim() === "") return;
    setBusy(true);
    setFailure(null);
    try {
      const document = startingDocument(channel, mode, category);
      const created = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/templates", {
          params: { path: { ws: workspaceId } },
          body: {
            name: name.trim(),
            channel,
            category: channel === "email" ? category : "transactional",
            ...(document ? { document: asBody(document) } : {}),
          },
        }),
      );
      void navigate(`/w/${workspaceId}/templates/${created.id}`);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="New template" onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <TextField
          label="Name"
          hint="Up to 100 characters. Unique among the live templates."
          value={name}
          onChange={(event) => setName(event.target.value)}
          maxLength={100}
          autoComplete="off"
          required
        />
        <SelectField
          label="Channel"
          value={channel}
          onChange={(event) => setChannel(event.target.value === "webhook" ? "webhook" : "email")}
          hint="It cannot be changed later."
        >
          <option value="email">Email</option>
          <option value="webhook">Webhook</option>
        </SelectField>
        {channel === "email" ? (
          <>
            <SelectField
              label="Category"
              value={category}
              onChange={(event) => setCategory(event.target.value === "transactional" ? "transactional" : "marketing")}
              hint="Marketing mail needs an unsubscribe link. It cannot be changed later."
            >
              <option value="marketing">Marketing</option>
              <option value="transactional">Transactional</option>
            </SelectField>
            <fieldset className="fl-fieldset">
              <legend className="field-label">How you write it</legend>
              <p className="field-hint">Chosen now and kept for the life of the template.</p>
              {MODES.map((entry) => (
                <label key={entry.id} className="check">
                  <input
                    type="radio"
                    name="template-mode"
                    value={entry.id}
                    checked={mode === entry.id}
                    disabled={!entry.available}
                    onChange={() => setMode(entry.id)}
                  />
                  <span>
                    {entry.label}
                    {entry.available ? null : <span className="faint"> (not available yet)</span>}
                    <span className="faint"> {entry.hint}</span>
                  </span>
                </label>
              ))}
            </fieldset>
          </>
        ) : (
          <p className="muted">A webhook template holds the headers and the JSON body of a call.</p>
        )}
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || name.trim() === ""}>
            {busy ? "Creating" : "Create template"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
