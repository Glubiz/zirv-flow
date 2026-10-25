import { describeError, useSenderOptions } from "../../api";
import { ErrorState, LoadingState, SelectField } from "../../ui";

/** The sender identities of the workspace as a choice. */
export function SenderPicker({
  workspaceId,
  value,
  onChange,
  disabled,
}: {
  workspaceId: string;
  value: string;
  onChange: (id: string) => void;
  disabled: boolean;
}) {
  const senders = useSenderOptions(workspaceId);
  if (senders.status === "loading") return <LoadingState label="Loading sender identities" />;
  if (senders.status === "error") {
    return (
      <div className="field">
        <span className="field-label">Sender identity</span>
        <p className="faint">{value === "" ? "No sender chosen." : <span className="mono">{value}</span>}</p>
        <ErrorState {...describeError(senders.error)} onRetry={senders.reload} />
      </div>
    );
  }
  const chosen = senders.data.find((sender) => sender.id === value);
  const live = senders.data.filter((sender) => !sender.archived);
  let hint = "A sender on a verified domain.";
  if (live.length === 0) hint = "No sender identity yet. Add one under sending.";
  if (value !== "" && chosen === undefined) hint = "Warning: the chosen sender is not in this workspace's list. Choose another.";
  if (chosen?.archived) hint = "Warning: the chosen sender is archived and cannot send. Choose another.";
  return (
    <SelectField
      label="Sender identity"
      value={value}
      onChange={(event) => onChange(event.target.value)}
      disabled={disabled}
      hint={hint}
    >
      <option value="">Choose a sender</option>
      {value !== "" && chosen === undefined ? <option value={value}>{value}</option> : null}
      {chosen?.archived ? (
        <option value={chosen.id}>
          {chosen.name} &lt;{chosen.from_address}&gt; (archived)
        </option>
      ) : null}
      {live.map((sender) => (
        <option key={sender.id} value={sender.id}>
          {sender.name} &lt;{sender.from_address}&gt;
        </option>
      ))}
    </SelectField>
  );
}
