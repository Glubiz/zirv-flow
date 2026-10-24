import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, TextField } from "../../ui";

export type Dimension = components["schemas"]["DimensionBody"];
export type DimensionValue = components["schemas"]["DimensionValueBody"];

interface DimensionDialogProps {
  workspaceId: string;
  /** The dimension being renamed; absent when declaring a new one. */
  dimension?: Dimension;
  onClose: () => void;
  onSaved: () => void;
}

/** Declares a dimension, or renames one. The key never changes. */
export function DimensionDialog({ workspaceId, dimension, onClose, onSaved }: DimensionDialogProps) {
  const [key, setKey] = useState("");
  const [name, setName] = useState(dimension?.name ?? "");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const incomplete = name.trim() === "" || (dimension === undefined && key.trim() === "");
  const unchanged = dimension !== undefined && name.trim() === dimension.name;

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      if (dimension) {
        await unwrap(
          client.PATCH("/api/v1/workspaces/{ws}/consent/dimensions/{key}", {
            params: { path: { ws: workspaceId, key: dimension.key } },
            body: { name: name.trim() },
          }),
        );
      } else {
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/consent/dimensions", {
            params: { path: { ws: workspaceId } },
            body: { key: key.trim(), name: name.trim() },
          }),
        );
      }
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={dimension ? `Rename ${dimension.name}` : "Add a dimension"} onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        {dimension ? (
          <p className="muted">
            Key: <span className="mono strong">{dimension.key}</span>. The key stays as it is.
          </p>
        ) : (
          <TextField
            label="Key"
            mono
            hint="Lowercase letters, digits, underscores and hyphens, up to 64. It cannot be renamed. topic is taken."
            value={key}
            onChange={(event) => setKey(event.target.value)}
            autoComplete="off"
            required
          />
        )}
        <TextField
          label="Name"
          hint="What people in your team see. Up to 100 characters."
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
            {busy ? "Saving" : dimension ? "Save name" : "Add dimension"}
          </Button>
        </div>
      </form>
    </Dialog>
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
  const [key, setKey] = useState("");
  const [name, setName] = useState(value?.name ?? "");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const incomplete = name.trim() === "" || (value === undefined && key.trim() === "");
  const unchanged = value !== undefined && name.trim() === value.name;

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      if (value) {
        await unwrap(
          client.PATCH("/api/v1/workspaces/{ws}/consent/dimensions/{key}/values/{value}", {
            params: { path: { ws: workspaceId, key: dimension.key, value: value.key } },
            body: { name: name.trim() },
          }),
        );
      } else {
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/consent/dimensions/{key}/values", {
            params: { path: { ws: workspaceId, key: dimension.key } },
            body: { key: key.trim(), name: name.trim() },
          }),
        );
      }
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title={value ? `Rename ${value.name}` : `Add a value to ${dimension.name}`} onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        {value ? (
          <p className="muted">
            Key: <span className="mono strong">{value.key}</span>. The key stays as it is.
          </p>
        ) : (
          <TextField
            label="Value key"
            mono
            hint="Lowercase letters, digits, underscores and hyphens, up to 64. It cannot be renamed."
            value={key}
            onChange={(event) => setKey(event.target.value)}
            autoComplete="off"
            required
          />
        )}
        <TextField
          label="Value name"
          hint="Up to 100 characters."
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
            {busy ? "Saving" : value ? "Save name" : "Add value"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
