import { useState, type SyntheticEvent } from "react";

import { client, describeError, unwrap, type components, type ErrorSummary } from "../../api";
import { Button, Dialog, InlineError, SelectField, TextField } from "../../ui";

export type Trait = components["schemas"]["TraitBody"];
type UpdateBody = components["schemas"]["UpdateTraitRequest"];

export const TRAIT_TYPES: { value: string; label: string }[] = [
  { value: "string", label: "Text" },
  { value: "number", label: "Number" },
  { value: "boolean", label: "Yes or no" },
  { value: "timestamp", label: "Date and time" },
  { value: "date", label: "Date" },
  { value: "string_list", label: "List of text" },
];

export const PRECEDENCES: { value: string; label: string; help: string }[] = [
  { value: "latest", label: "Latest", help: "The most recently observed value wins" },
  { value: "source_priority", label: "Source priority", help: "The first source in your list that has a value wins" },
  { value: "first", label: "First", help: "The earliest observed value wins" },
];

export function typeLabel(value: string): string {
  return TRAIT_TYPES.find((type) => type.value === value)?.label ?? value;
}

export function precedenceLabel(value: string): string {
  return PRECEDENCES.find((precedence) => precedence.value === value)?.label ?? value;
}

function parseSources(text: string): string[] {
  return text
    .split(",")
    .map((source) => source.trim())
    .filter((source) => source !== "");
}

interface TraitDialogProps {
  workspaceId: string;
  /** The trait being edited; absent when creating. */
  trait?: Trait;
  onClose: () => void;
  onSaved: () => void;
}

export function TraitDialog({ workspaceId, trait, onClose, onSaved }: TraitDialogProps) {
  const [key, setKey] = useState("");
  const [type, setType] = useState("string");
  const [precedence, setPrecedence] = useState(trait?.precedence ?? "latest");
  const [sources, setSources] = useState((trait?.source_order ?? []).join(", "));
  const [pii, setPii] = useState(trait?.pii ?? false);
  const [channelAddress, setChannelAddress] = useState(trait?.channel_address ?? false);
  const [description, setDescription] = useState(trait?.description ?? "");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const sourceOrder = precedence === "source_priority" ? parseSources(sources) : (trait?.source_order ?? []);

  function changes(): UpdateBody {
    if (!trait) {
      return {};
    }
    const body: UpdateBody = {};
    if (precedence !== trait.precedence) body.precedence = precedence;
    if (precedence === "source_priority" && sourceOrder.join("\n") !== trait.source_order.join("\n")) {
      body.source_order = sourceOrder;
    }
    if (pii !== trait.pii) body.pii = pii;
    if (channelAddress !== trait.channel_address) body.channel_address = channelAddress;
    if (description !== trait.description) body.description = description;
    return body;
  }

  const unchanged = trait !== undefined && Object.keys(changes()).length === 0;
  const incomplete = trait === undefined && key.trim() === "";

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      if (trait) {
        await unwrap(
          client.PATCH("/api/v1/workspaces/{ws}/traits/{key}", {
            params: { path: { ws: workspaceId, key: trait.key } },
            body: changes(),
          }),
        );
      } else {
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/traits", {
            params: { path: { ws: workspaceId } },
            body: {
              key: key.trim(),
              type,
              precedence,
              source_order: precedence === "source_priority" ? parseSources(sources) : [],
              pii,
              channel_address: channelAddress,
              description,
            },
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
    <Dialog title={trait ? `Edit ${trait.key}` : "Add a field"} onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        {trait ? (
          <p className="muted">
            Type: <span className="strong">{typeLabel(trait.type)}</span>. A field keeps its type. To use another type,
            archive this field and add a new one.
          </p>
        ) : (
          <>
            <TextField
              label="Key"
              mono
              hint="Lowercase letters, digits and underscores, starting with a letter. It cannot be renamed."
              value={key}
              onChange={(event) => setKey(event.target.value)}
              autoComplete="off"
              required
            />
            <SelectField
              label="Type"
              hint="It cannot be changed later."
              value={type}
              onChange={(event) => setType(event.target.value)}
            >
              {TRAIT_TYPES.map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </SelectField>
          </>
        )}
        <SelectField
          label="When sources disagree"
          hint={PRECEDENCES.find((option) => option.value === precedence)?.help}
          value={precedence}
          onChange={(event) => setPrecedence(event.target.value)}
        >
          {PRECEDENCES.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </SelectField>
        {precedence === "source_priority" ? (
          <TextField
            label="Source order"
            hint="Source names, highest priority first, separated by commas."
            value={sources}
            onChange={(event) => setSources(event.target.value)}
            autoComplete="off"
          />
        ) : null}
        <fieldset className="checks">
          <legend>Handling</legend>
          <label className="check">
            <input type="checkbox" checked={pii} onChange={(event) => setPii(event.target.checked)} />
            <span>
              Personal data <span className="faint">Kept out of logs and exports by default</span>
            </span>
          </label>
          <label className="check">
            <input
              type="checkbox"
              checked={channelAddress}
              onChange={(event) => setChannelAddress(event.target.checked)}
            />
            <span>
              Channel address <span className="faint">Where messages are sent, such as an email address</span>
            </span>
          </label>
        </fieldset>
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
            {busy ? "Saving" : trait ? "Save changes" : "Add field"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
