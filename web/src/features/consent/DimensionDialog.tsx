import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, TextField } from "../../ui";

export type Dimension = components["schemas"]["DimensionBody"];
export type DimensionValue = components["schemas"]["DimensionValueBody"];

interface KeyNameDialogProps {
  title: string;
  keyLabel: string;
  keyHint: string;
  nameLabel: string;
  nameHint: string;
  addLabel: string;
  /** The item being renamed; absent when adding one. */
  existing?: { key: string; name: string };
  /** Adds the item, or renames the existing one. */
  save: (key: string, name: string) => Promise<unknown>;
  onClose: () => void;
  onSaved: () => void;
}

/** A dialog for the two fields a dimension and a dimension value share: a key that is set once, and a name. */
function KeyNameDialog({ title, keyLabel, keyHint, nameLabel, nameHint, addLabel, existing, save, onClose, onSaved }: KeyNameDialogProps) {
  const [key, setKey] = useState("");
  const [name, setName] = useState(existing?.name ?? "");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const incomplete = name.trim() === "" || (existing === undefined && key.trim() === "");
  const unchanged = existing !== undefined && name.trim() === existing.name;

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      await save(key.trim(), name.trim());
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={title} onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        {existing ? (
          <p className="muted">
            Key: <span className="mono strong">{existing.key}</span>. The key stays as it is.
          </p>
        ) : (
          <TextField label={keyLabel} mono hint={keyHint} value={key} onChange={(event) => setKey(event.target.value)} autoComplete="off" required />
        )}
        <TextField
          label={nameLabel}
          hint={nameHint}
          value={name}
          onChange={(event) => setName(event.target.value)}
          maxLength={100}
          autoComplete="off"
          required
        />
        {failure ? <InlineError {...failure} /> : null}
        <div className="dialog-actions">
          <Button onClick={onClose}>Cancel</Button>
          <Button type="submit" variant="primary" disabled={busy || incomplete || unchanged}>
            {busy ? "Saving" : existing ? "Save name" : addLabel}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}

interface DimensionDialogProps {
  workspaceId: string;
  /** The dimension being renamed; absent when declaring a new one. */
  dimension?: Dimension;
  onClose: () => void;
  onSaved: () => void;
}

/** Declares a dimension, or renames one. The key never changes. */
export function DimensionDialog({ workspaceId, dimension, onClose, onSaved }: DimensionDialogProps) {
  return (
    <KeyNameDialog
      title={dimension ? `Rename ${dimension.name}` : "Add a dimension"}
      keyLabel="Key"
      keyHint="Lowercase letters, digits, underscores and hyphens, up to 64. It cannot be renamed. topic is taken."
      nameLabel="Name"
      nameHint="What people in your team see. Up to 100 characters."
      addLabel="Add dimension"
      existing={dimension}
      save={(key, name) =>
        dimension
          ? unwrap(
              client.PATCH("/api/v1/workspaces/{ws}/consent/dimensions/{key}", {
                params: { path: { ws: workspaceId, key: dimension.key } },
                body: { name },
              }),
            )
          : unwrap(
              client.POST("/api/v1/workspaces/{ws}/consent/dimensions", {
                params: { path: { ws: workspaceId } },
                body: { key, name },
              }),
            )
      }
      onClose={onClose}
      onSaved={onSaved}
    />
  );
}

interface ValueDialogProps {
  workspaceId: string;
  dimension: Dimension;
  /** The value being renamed; absent when adding one. */
  value?: DimensionValue;
  onClose: () => void;
  onSaved: () => void;
}

/** Adds a value to a dimension, or renames one. */
export function ValueDialog({ workspaceId, dimension, value, onClose, onSaved }: ValueDialogProps) {
  return (
    <KeyNameDialog
      title={value ? `Rename ${value.name}` : `Add a value to ${dimension.name}`}
      keyLabel="Value key"
      keyHint="Lowercase letters, digits, underscores and hyphens, up to 64. It cannot be renamed."
      nameLabel="Value name"
      nameHint="Up to 100 characters."
      addLabel="Add value"
      existing={value}
      save={(key, name) =>
        value
          ? unwrap(
              client.PATCH("/api/v1/workspaces/{ws}/consent/dimensions/{key}/values/{value}", {
                params: { path: { ws: workspaceId, key: dimension.key, value: value.key } },
                body: { name },
              }),
            )
          : unwrap(
              client.POST("/api/v1/workspaces/{ws}/consent/dimensions/{key}/values", {
                params: { path: { ws: workspaceId, key: dimension.key } },
                body: { key, name },
              }),
            )
      }
      onClose={onClose}
      onSaved={onSaved}
    />
  );
}
