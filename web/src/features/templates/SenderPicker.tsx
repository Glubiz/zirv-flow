import { client, describeError, unwrap, useResource } from "../../api";
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
  const senders = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/sender-identities", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  if (senders.status === "loading") return <LoadingState label="Loading sender identities" />;
  if (senders.status === "error") return <ErrorState {...describeError(senders.error)} onRetry={senders.reload} />;
  const known = senders.data.some((sender) => sender.id === value);
  return (
    <SelectField
      label="Sender identity"
      value={value}
      onChange={(event) => onChange(event.target.value)}
      disabled={disabled}
      hint={senders.data.length === 0 ? "No sender identity yet. Add one under sending." : "A sender on a verified domain."}
    >
      <option value="">Choose a sender</option>
      {value !== "" && !known ? <option value={value}>{value}</option> : null}
      {senders.data.map((sender) => (
        <option key={sender.id} value={sender.id}>
          {sender.from_name} &lt;{sender.from_address}&gt;
        </option>
      ))}
    </SelectField>
  );
}
