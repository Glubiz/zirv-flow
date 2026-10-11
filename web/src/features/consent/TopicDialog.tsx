import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, SelectField, TextField } from "../../ui";
import { policyHelp, POLICIES } from "./labels";

export type Topic = components["schemas"]["TopicBody"];
type UpdateBody = components["schemas"]["UpdateTopicRequest"];

interface TopicDialogProps {
  workspaceId: string;
  /** The topic being edited; absent when creating. */
  topic?: Topic;
  onClose: () => void;
  onSaved: () => void;
}

export function TopicDialog({ workspaceId, topic, onClose, onSaved }: TopicDialogProps) {
  const [key, setKey] = useState("");
  const [name, setName] = useState(topic?.name ?? "");
  const [description, setDescription] = useState(topic?.description ?? "");
  const [policy, setPolicy] = useState(topic?.policy ?? "opt_out");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  function changes(): UpdateBody {
    if (!topic) {
      return {};
    }
    const body: UpdateBody = {};
    if (name !== topic.name) body.name = name;
    if (description !== topic.description) body.description = description;
    if (policy !== topic.policy) body.policy = policy;
    return body;
  }

  const unchanged = topic !== undefined && Object.keys(changes()).length === 0;
  const incomplete = name.trim() === "" || (topic === undefined && key.trim() === "");

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      if (topic) {
        await unwrap(
          client.PATCH("/api/v1/workspaces/{ws}/topics/{id}", {
            params: { path: { ws: workspaceId, id: topic.id } },
            body: changes(),
          }),
        );
      } else {
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/topics", {
            params: { path: { ws: workspaceId } },
            body: { key: key.trim(), name: name.trim(), description, channel: "email", policy },
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
    <Dialog title={topic ? `Edit ${topic.name}` : "Add a topic"} onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        {topic ? (
          <p className="muted">
            Key: <span className="mono strong">{topic.key}</span>. The key and the channel stay as they are.
          </p>
        ) : (
          <TextField
            label="Key"
            mono
            hint="Lowercase letters, digits, underscores and hyphens, up to 64. It cannot be renamed."
            value={key}
            onChange={(event) => setKey(event.target.value)}
            autoComplete="off"
            required
          />
        )}
        <TextField
          label="Name"
          hint="What people see on a preference page."
          value={name}
          onChange={(event) => setName(event.target.value)}
          maxLength={100}
          autoComplete="off"
          required
        />
        <SelectField label="Without an answer" hint={policyHelp(policy)} value={policy} onChange={(event) => setPolicy(event.target.value)}>
          {POLICIES.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </SelectField>
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
          <Button type="submit" variant="primary" disabled={busy || unchanged || incomplete}>
            {busy ? "Saving" : topic ? "Save changes" : "Add topic"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
