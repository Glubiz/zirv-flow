import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, Panel, SelectField, TextField } from "../../ui";
import { KINDS, reasonLabel } from "./labels";

type Check = components["schemas"]["CheckBody"];

interface CheckPanelProps {
  workspaceId: string;
  canManage: boolean;
  /** Called after a suppression was lifted. */
  onRemoved: (message: string) => void;
}

export function CheckPanel({ workspaceId, canManage, onRemoved }: CheckPanelProps) {
  const [kind, setKind] = useState("email");
  const [value, setValue] = useState("");
  const [checked, setChecked] = useState<{ kind: string; value: string; result: Check } | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const result = checked !== null && checked.kind === kind && checked.value === value.trim() ? checked.result : null;

  async function check(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      const answer = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/suppressions/check", {
          params: { path: { ws: workspaceId } },
          body: { kind, value: value.trim() },
        }),
      );
      setChecked({ kind, value: value.trim(), result: answer });
    } catch (error) {
      setChecked(null);
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(
        client.POST("/api/v1/workspaces/{ws}/suppressions/remove", {
          params: { path: { ws: workspaceId } },
          body: { kind, value: value.trim() },
        }),
      );
      setConfirming(false);
      setChecked(null);
      setValue("");
      onRemoved("The suppression was removed. Messages can be sent to it again, subject to consent.");
    } catch (error) {
      setConfirming(false);
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Panel title="Check an address">
      <form className="row" onSubmit={(event) => void check(event)} noValidate>
        <SelectField label="Kind" value={kind} onChange={(event) => setKind(event.target.value)}>
          {KINDS.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </SelectField>
        <div className="row-grow">
          <TextField
            label="Address or number"
            hint="Checked by its hash. It is not stored or logged."
            value={value}
            onChange={(event) => setValue(event.target.value)}
            autoComplete="off"
          />
        </div>
        <Button type="submit" disabled={busy || value.trim() === ""}>
          Check
        </Button>
      </form>
      {failure ? <InlineError {...failure} /> : null}
      {result ? (
        <div className="notice" role="status">
          {result.suppressed ? (
            <>
              <p className="strong">Suppressed: {reasonLabel(result.reason ?? "")}</p>
              {result.reason === "erased" ? (
                <p className="muted">Erased suppressions are permanent and cannot be removed.</p>
              ) : canManage ? (
                <Button onClick={() => setConfirming(true)}>Remove suppression</Button>
              ) : null}
            </>
          ) : (
            <p className="strong">Not suppressed</p>
          )}
        </div>
      ) : null}
      {confirming ? (
        <Dialog title="Remove this suppression?" onClose={() => setConfirming(false)}>
          <p className="muted">
            Messages can be sent to this address again once the person has also consented. If it was suppressed because of a bounce
            or a complaint, sending again may hurt your sender reputation.
          </p>
          <div className="dialog-actions">
            <Button onClick={() => setConfirming(false)}>Keep it suppressed</Button>
            <Button variant="primary" onClick={() => void remove()} disabled={busy}>
              {busy ? "Removing" : "Remove suppression"}
            </Button>
          </div>
        </Dialog>
      ) : null}
    </Panel>
  );
}
