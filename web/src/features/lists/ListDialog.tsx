import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, TextField } from "../../ui";

export type List = components["schemas"]["ListBody"];

interface ListDialogProps {
  workspaceId: string;
  /** The list being renamed; absent when creating. */
  list?: List;
  onClose: () => void;
  onSaved: (list: List) => void;
}

export function ListDialog({ workspaceId, list, onClose, onSaved }: ListDialogProps) {
  const [name, setName] = useState(list?.name ?? "");
  const [description, setDescription] = useState(list?.description ?? "");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const trimmed = name.trim();
  const unchanged = list !== undefined && trimmed === list.name && description === list.description;

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      const saved = list
        ? await unwrap(
            client.PATCH("/api/v1/workspaces/{ws}/lists/{id}", {
              params: { path: { ws: workspaceId, id: list.id } },
              body: { name: trimmed, description },
            }),
          )
        : await unwrap(
            client.POST("/api/v1/workspaces/{ws}/lists", {
              params: { path: { ws: workspaceId } },
              body: { name: trimmed, description },
            }),
          );
      onSaved(saved);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={list ? `Edit ${list.name}` : "New list"} onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <TextField
          label="Name"
          hint="Up to 128 characters. Two lists cannot share a name."
          value={name}
          onChange={(event) => setName(event.target.value)}
          maxLength={128}
          autoComplete="off"
          required
        />
        <TextField
          label="Description"
          hint="Optional. Up to 500 characters."
          value={description}
          onChange={(event) => setDescription(event.target.value)}
          maxLength={500}
        />
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || trimmed === "" || unchanged}>
            {busy ? "Saving" : list ? "Save changes" : "Create list"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
