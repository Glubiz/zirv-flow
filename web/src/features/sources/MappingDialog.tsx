import { useId, useState } from "react";

import { client, describeError, unwrap, useResource, type components, type ErrorSummary } from "../../api";
import {
  Button,
  Dialog,
  EmptyState,
  ErrorState,
  InlineError,
  LoadingState,
  Pill,
  TextField,
  TransformEditor,
  type TransformDraft,
} from "../../ui";
import {
  draftProblems,
  draftToMapping,
  mappingToDraft,
  MAX_EVENT_PROPERTIES,
  newProperty,
  newRule,
  type EventDraft,
  type IdentifierChoice,
  type MappingDraft,
  type PropertyDraft,
  type RuleDraft,
  type TargetChoice,
} from "./mapping";
import type { Stream } from "./labels";

type Trait = components["schemas"]["TraitBody"];
type ListItem = components["schemas"]["ListBody"];
type Preview = components["schemas"]["PreviewBody"];
type PreviewRecord = components["schemas"]["PreviewRecord"];

const IDENTIFIER_LABELS: Record<IdentifierChoice, string> = {
  email: "Email address",
  phone: "Phone number",
  anonymous_id: "Anonymous ID",
  external: "External ID…",
};

const REASON_TEXT: Record<string, string> = {
  missing_identifier: "no identifier had a value",
  invalid_identifier: "the identifier is not valid",
  wrong_type: "the value does not fit its type",
  bad_timestamp: "the date or time could not be read",
  unknown_column: "the stream has no such field",
  invalid_key: "the key is too long",
};

function choiceValue(choice: TargetChoice): string {
  switch (choice.kind) {
    case "identifier":
      return `identifier:${choice.identifier}`;
    case "trait":
      return `trait:${choice.key}`;
    case "list":
      return `list:${choice.listId}`;
  }
}

function choiceFrom(value: string, previous: TargetChoice): TargetChoice {
  if (value.startsWith("trait:")) return { kind: "trait", key: value.slice("trait:".length) };
  if (value.startsWith("list:")) return { kind: "list", listId: value.slice("list:".length) };
  return {
    kind: "identifier",
    identifier: value.slice("identifier:".length) as IdentifierChoice,
    namespace: previous.kind === "identifier" ? previous.namespace : "",
  };
}

interface MappingDialogProps {
  workspaceId: string;
  sourceId: string;
  stream: Stream;
  onClose: () => void;
  onSaved: (stream: Stream) => void;
}

/** Loads the registry and the lists the mapping can point at, then shows the editor. */
export function MappingDialog(props: MappingDialogProps) {
  const { workspaceId, stream, onClose } = props;
  const traits = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/traits", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const lists = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/lists", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  let body;
  if (traits.status === "ready" && lists.status === "ready") {
    body = <Editor {...props} traits={traits.data.filter((item) => !item.archived)} lists={lists.data} />;
  } else {
    const failed = [traits, lists].find((resource) => resource.status === "error");
    body =
      failed?.status === "error" ? (
        <ErrorState {...describeError(failed.error)} onRetry={failed.reload} />
      ) : (
        <LoadingState label="Loading your fields and lists" />
      );
  }
  return (
    <Dialog title={`Map ${stream.name}`} onClose={onClose} wide>
      {body}
    </Dialog>
  );
}

interface EditorProps extends MappingDialogProps {
  traits: Trait[];
  lists: ListItem[];
}

function Editor({ workspaceId, sourceId, stream, traits, lists, onClose, onSaved }: EditorProps) {
  const fieldNames = stream.discovered.fields.map((field) => field.name);
  const fieldsId = useId();
  const [draft, setDraft] = useState<MappingDraft>(() => mappingToDraft(fieldNames, stream.mapping));
  const [busy, setBusy] = useState(false);
  const [previewing, setPreviewing] = useState(false);
  const [preview, setPreview] = useState<Preview | null>(null);
  const [previewFailure, setPreviewFailure] = useState<ErrorSummary | null>(null);
  const [saveFailure, setSaveFailure] = useState<ErrorSummary | null>(null);

  const hasKey = stream.primary_key.length > 0;
  const problems = draftProblems(draft, hasKey);

  function edit(next: MappingDraft) {
    setDraft(next);
    setPreview(null);
  }

  function changeRule(id: number, change: Partial<RuleDraft>) {
    edit({ ...draft, rules: draft.rules.map((rule) => (rule.id === id ? { ...rule, ...change } : rule)) });
  }

  function changeEvent(change: Partial<EventDraft>) {
    edit({ ...draft, event: { ...draft.event, ...change } });
  }

  function changeProperty(id: number, change: Partial<PropertyDraft>) {
    changeEvent({
      properties: draft.event.properties.map((property) => (property.id === id ? { ...property, ...change } : property)),
    });
  }

  async function runPreview() {
    setPreviewing(true);
    setPreviewFailure(null);
    try {
      setPreview(
        await unwrap(
          client.POST("/api/v1/workspaces/{ws}/sources/{id}/streams/{name}/preview", {
            params: { path: { ws: workspaceId, id: sourceId, name: stream.name } },
            body: { mapping: draftToMapping(draft) },
          }),
        ),
      );
    } catch (error) {
      setPreview(null);
      setPreviewFailure(describeError(error));
    } finally {
      setPreviewing(false);
    }
  }

  async function save() {
    setBusy(true);
    setSaveFailure(null);
    try {
      const saved = await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/sources/{id}/streams/{name}", {
          params: { path: { ws: workspaceId, id: sourceId, name: stream.name } },
          body: {
            enabled: stream.enabled,
            sync_mode: stream.sync_mode,
            cursor_field: stream.cursor_field,
            primary_key: stream.primary_key,
            staleness_threshold_seconds: stream.staleness_threshold_seconds,
            mapping: draftToMapping(draft),
          },
        }),
      );
      onSaved(saved);
    } catch (error) {
      setSaveFailure(describeError(error));
      setBusy(false);
    }
  }

  const fieldsList = (
    <datalist id={fieldsId}>
      {fieldNames.map((name) => (
        <option key={name} value={name} />
      ))}
    </datalist>
  );

  return (
    <div className="stack">
      {fieldsList}
      <p className="muted">
        Choose what each field of a record becomes. Name a field of the stream, or reach into a nested one with dots, such as
        address.city or items.0.sku.
      </p>

      <section className="stack-tight" aria-label="People">
        <h3>People</h3>
        {traits.length === 0 ? (
          <p className="notice">
            Your account has no fields yet, so a row can only identify a person or add them to a list. Add fields under Fields first
            to keep values on people.
          </p>
        ) : null}
        {lists.length === 0 ? (
          <p className="notice">There are no lists yet. Create one under Lists to add people to it from this stream.</p>
        ) : null}
        <div className="table-scroll">
          <table className="table">
            <caption>Fields of a record</caption>
            <thead>
              <tr>
                <th scope="col">Field</th>
                <th scope="col">Becomes</th>
                <th scope="col">Steps</th>
                <th scope="col">
                  <span className="visually-hidden">Remove</span>
                </th>
              </tr>
            </thead>
            <tbody>
              {draft.rules.map((rule, index) => {
                const label = rule.column === "" ? `row ${index + 1}` : rule.column;
                return (
                  <tr key={rule.id}>
                    <td>
                      <input
                        className="input input-mono"
                        aria-label={`Field of row ${index + 1}`}
                        list={fieldsId}
                        value={rule.column}
                        disabled={busy}
                        onChange={(event) => changeRule(rule.id, { column: event.target.value })}
                      />
                    </td>
                    <td>
                      <div className="stack-tight">
                        <select
                          className="select"
                          aria-label={`What ${label} becomes`}
                          value={choiceValue(rule.target)}
                          disabled={busy}
                          onChange={(event) =>
                            changeRule(rule.id, { target: choiceFrom(event.target.value, rule.target) })
                          }
                        >
                          <optgroup label="Identifies a person">
                            {(Object.keys(IDENTIFIER_LABELS) as IdentifierChoice[]).map((kind) => (
                              <option key={kind} value={`identifier:${kind}`}>
                                {IDENTIFIER_LABELS[kind]}
                              </option>
                            ))}
                          </optgroup>
                          <optgroup label="Fills one of your fields">
                            {rule.target.kind === "trait" && rule.target.key === "" ? (
                              <option value="trait:">Choose a field…</option>
                            ) : null}
                            {traits.map((item) => (
                              <option key={item.key} value={`trait:${item.key}`}>
                                {item.key}
                              </option>
                            ))}
                          </optgroup>
                          <optgroup label="Adds to a list when the value is yes">
                            {rule.target.kind === "list" && rule.target.listId === "" ? (
                              <option value="list:">Choose a list…</option>
                            ) : null}
                            {lists.map((item) => (
                              <option key={item.id} value={`list:${item.id}`}>
                                {item.name}
                              </option>
                            ))}
                          </optgroup>
                        </select>
                        {rule.target.kind === "identifier" && rule.target.identifier === "external" ? (
                          <input
                            className="input input-mono"
                            aria-label={`Name of the external ID in ${label}`}
                            placeholder="crm"
                            value={rule.target.namespace}
                            disabled={busy}
                            onChange={(event) =>
                              changeRule(rule.id, {
                                target: { kind: "identifier", identifier: "external", namespace: event.target.value },
                              })
                            }
                          />
                        ) : null}
                      </div>
                    </td>
                    <td>
                      <details>
                        <summary>
                          Steps ({rule.transforms.length})
                          <span className="visually-hidden"> for {label}</span>
                        </summary>
                        <TransformEditor
                          column={label}
                          others={fieldNames.filter((name) => name !== rule.column)}
                          steps={rule.transforms}
                          disabled={busy}
                          onChange={(steps: TransformDraft[]) => changeRule(rule.id, { transforms: steps })}
                        />
                      </details>
                    </td>
                    <td>
                      <Button
                        onClick={() => edit({ ...draft, rules: draft.rules.filter((other) => other.id !== rule.id) })}
                        disabled={busy}
                        aria-label={`Remove row ${index + 1}`}
                      >
                        Remove
                      </Button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
        <div>
          <Button onClick={() => edit({ ...draft, rules: [...draft.rules, newRule()] })} disabled={busy}>
            Add a row
          </Button>
        </div>
      </section>

      <section className="stack-tight" aria-label="Event">
        <h3>Event</h3>
        <label className="check">
          <input
            type="checkbox"
            checked={draft.event.enabled}
            disabled={busy || !hasKey}
            onChange={(event) => changeEvent({ enabled: event.target.checked })}
          />
          <span>Each record is also an event</span>
        </label>
        {!hasKey ? (
          <p className="field-hint">
            Choose a primary key for this stream first (Set up). An event is kept once per record, and the key is how a record is
            recognised when the file is read again.
          </p>
        ) : null}
        {draft.event.enabled ? (
          <div className="stack-tight">
            <TextField
              label="Event name"
              value={draft.event.name}
              disabled={busy}
              onChange={(event) => changeEvent({ name: event.target.value })}
            />
            <TextField
              label="Time field"
              hint="Optional. Without it the event happened when the run read it, and an old file's events are flagged late."
              list={fieldsId}
              value={draft.event.timestampColumn}
              disabled={busy}
              onChange={(event) => changeEvent({ timestampColumn: event.target.value })}
            />
            {draft.event.timestampColumn.trim() !== "" ? (
              <details>
                <summary>Steps for the time field ({draft.event.timestampTransforms.length})</summary>
                <TransformEditor
                  column="the time"
                  others={fieldNames}
                  steps={draft.event.timestampTransforms}
                  disabled={busy}
                  onChange={(steps: TransformDraft[]) => changeEvent({ timestampTransforms: steps })}
                />
              </details>
            ) : null}
            <h4>Properties</h4>
            {draft.event.properties.length === 0 ? <p className="faint">The event carries no properties.</p> : null}
            {draft.event.properties.map((property, index) => (
              <div key={property.id} className="row">
                <input
                  className="input"
                  aria-label={`Name of property ${index + 1}`}
                  value={property.property}
                  disabled={busy}
                  onChange={(event) => changeProperty(property.id, { property: event.target.value })}
                />
                <input
                  className="input input-mono"
                  aria-label={`Field of property ${index + 1}`}
                  list={fieldsId}
                  value={property.column}
                  disabled={busy}
                  onChange={(event) => changeProperty(property.id, { column: event.target.value })}
                />
                <details>
                  <summary>
                    Steps ({property.transforms.length})
                    <span className="visually-hidden"> for property {index + 1}</span>
                  </summary>
                  <TransformEditor
                    column={`property ${index + 1}`}
                    others={fieldNames}
                    steps={property.transforms}
                    disabled={busy}
                    onChange={(steps: TransformDraft[]) => changeProperty(property.id, { transforms: steps })}
                  />
                </details>
                <Button
                  onClick={() =>
                    changeEvent({ properties: draft.event.properties.filter((other) => other.id !== property.id) })
                  }
                  disabled={busy}
                  aria-label={`Remove property ${index + 1}`}
                >
                  Remove
                </Button>
              </div>
            ))}
            <div>
              <Button
                onClick={() => changeEvent({ properties: [...draft.event.properties, newProperty()] })}
                disabled={busy || draft.event.properties.length >= MAX_EVENT_PROPERTIES}
              >
                Add a property
              </Button>
            </div>
          </div>
        ) : null}
      </section>

      <section className="stack-tight" aria-label="Preview">
        <h3>Preview</h3>
        {problems.length > 0 ? (
          <ul className="list-plain attention" aria-label="Before you can preview or save">
            {problems.map((problem) => (
              <li key={problem}>{problem}</li>
            ))}
          </ul>
        ) : null}
        <div>
          <Button onClick={() => void runPreview()} disabled={busy || previewing || problems.length > 0}>
            {previewing ? "Reading the source" : "Preview"}
          </Button>
        </div>
        {previewing ? <LoadingState label="Reading the first records" /> : null}
        {previewFailure ? <InlineError {...previewFailure} /> : null}
        {preview === null && !previewing && !previewFailure ? (
          <p className="faint">
            Preview reads the first records from the source and shows what the mapping makes of them. Nothing is saved.
          </p>
        ) : null}
        {preview !== null && preview.records.length === 0 ? (
          <EmptyState title="The source returned no records">There is nothing to map yet.</EmptyState>
        ) : null}
        {preview !== null && preview.records.length > 0 ? <PreviewTable preview={preview} lists={lists} /> : null}
      </section>

      {saveFailure ? <InlineError {...saveFailure} /> : null}
      <div className="dialog-actions">
        <Button onClick={onClose}>Cancel</Button>
        <Button variant="primary" onClick={() => void save()} disabled={busy || problems.length > 0}>
          {busy ? "Saving" : "Save mapping"}
        </Button>
      </div>
    </div>
  );
}

function recordTone(result: string): "neutral" | "attention" | "error" {
  return result === "rejected" ? "error" : result === "deleted" ? "attention" : "neutral";
}

function PreviewTable({ preview, lists }: { preview: Preview; lists: ListItem[] }) {
  const listName = (id: string) => lists.find((item) => item.id === id)?.name ?? id;
  return (
    <>
      <div className="table-scroll">
        <table className="table">
          <caption>What the mapping makes of the first records</caption>
          <thead>
            <tr>
              <th scope="col">Record</th>
              <th scope="col">Result</th>
              <th scope="col">Identifiers</th>
              <th scope="col">Fields</th>
              <th scope="col">Lists</th>
              <th scope="col">Event</th>
            </tr>
          </thead>
          <tbody>
            {preview.records.map((record) => (
              <PreviewRow key={record.key} record={record} listName={listName} />
            ))}
          </tbody>
        </table>
      </div>
      {preview.truncated ? <p className="faint">The stream has more records than the {preview.read} shown.</p> : null}
    </>
  );
}

function PreviewRow({ record, listName }: { record: PreviewRecord; listName: (id: string) => string }) {
  const traits = Object.entries(record.traits);
  return (
    <tr>
      <td className="strong">{record.key}</td>
      <td>
        <Pill tone={recordTone(record.result)}>{record.result}</Pill>
        {record.errors.length > 0 ? (
          <ul className="list-plain">
            {record.errors.map((error) => (
              <li key={`${error.field}-${error.reason}`}>
                {error.field === "" ? "Record" : error.field}: {REASON_TEXT[error.reason] ?? error.reason}
              </li>
            ))}
          </ul>
        ) : null}
      </td>
      <td>{record.identifiers.map((item) => `${item.kind}: ${item.value}`).join(", ")}</td>
      <td>{traits.map(([key, value]) => `${key}: ${typeof value === "string" ? value : JSON.stringify(value)}`).join(", ")}</td>
      <td>{record.lists.map(listName).join(", ")}</td>
      <td>
        {record.event ? (
          <>
            {record.event.name}
            {record.event.occurred_at ? ` at ${record.event.occurred_at}` : ""}
            <br />
            <span className="faint">{JSON.stringify(record.event.properties)}</span>
          </>
        ) : null}
      </td>
    </tr>
  );
}
