import { useState, type SyntheticEvent } from "react";
import { Link, useParams } from "react-router";

import { ApiError, client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, InlineError, PageHeader, Panel, Pill, ResourceView, SelectField, TextField } from "../../ui";
import {
  IDENTIFIER_KINDS,
  identifierLabel,
  reasonText,
  useContactAccess,
  useContactSchema,
  type ContactSchema,
  type DefinitionField,
  type Trait,
} from "./schema";

export function ContactDefinitionPage() {
  const { ws = "" } = useParams();
  return <Definition key={ws} workspaceId={ws} />;
}

function typeName(trait: Trait | undefined): string {
  return trait?.type ?? "unknown";
}

function Definition({ workspaceId }: { workspaceId: string }) {
  const { canDefine } = useContactAccess(workspaceId);
  const schema = useContactSchema(workspaceId);

  return (
    <div className="page">
      <PageHeader
        title="Contact definition"
        lede="Which fields a contact has. Each field is a field from your registry, which gives it its type. A required field must be present on every contact; a field marked as an identifier also identifies the person."
        actions={
          <Link className="btn" to={`/w/${workspaceId}/contacts`}>
            Back to contacts
          </Link>
        }
      />
      <ResourceView resource={schema} loadingLabel="Loading the definition">
        {(data) =>
          canDefine ? (
            <Editor key={data.updatedAt ?? "new"} workspaceId={workspaceId} schema={data} onSaved={schema.reload} />
          ) : (
            <ReadOnly schema={data} />
          )
        }
      </ResourceView>
    </div>
  );
}

function ReadOnly({ schema }: { schema: ContactSchema }) {
  return (
    <>
      <p className="muted">Only workspace admins can change the definition.</p>
      <Panel flush>
        {schema.fields.length === 0 ? (
          <p className="panel-pad muted">
            {schema.defined ? "The definition has no fields." : "No definition has been saved yet. Contacts carry only a key."}
          </p>
        ) : (
          <div className="table-scroll">
            <table className="table">
              <caption>Fields of a contact, in order</caption>
              <thead>
                <tr>
                  <th scope="col">Field</th>
                  <th scope="col">Type</th>
                  <th scope="col">Required</th>
                  <th scope="col">Identifier</th>
                </tr>
              </thead>
              <tbody>
                {schema.fields.map((field) => (
                  <tr key={field.key}>
                    <td className="strong mono">{field.key}</td>
                    <td>{field.type}</td>
                    <td>{field.required ? "Yes" : "No"}</td>
                    <td>{field.identifier ? identifierLabel(field.identifier) : "—"}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </>
  );
}

/** A row of the editor: the definition field, with an identifier split into kind and system. */
interface Row {
  key: string;
  required: boolean;
  /** `""` for none, or `email`, `phone`, `anonymous_id` or `external`. */
  kind: string;
  /** The system name of an `external` identifier. */
  system: string;
}

function toRow(field: ContactSchema["fields"][number]): Row {
  const external = field.identifier?.startsWith("external:") ?? false;
  return {
    key: field.key,
    required: field.required,
    kind: external ? "external" : (field.identifier ?? ""),
    system: external ? (field.identifier ?? "").slice("external:".length) : "",
  };
}

function toField(row: Row): DefinitionField {
  return {
    key: row.key,
    required: row.required,
    identifier: row.kind === "" ? null : row.kind === "external" ? `external:${row.system.trim()}` : row.kind,
  };
}

function Editor({
  workspaceId,
  schema,
  onSaved,
}: {
  workspaceId: string;
  schema: ContactSchema;
  onSaved: () => void;
}) {
  const [rows, setRows] = useState<Row[]>(schema.fields.map(toRow));
  const [adding, setAdding] = useState("");
  const [problems, setProblems] = useState<Record<number, string>>({});
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const free = schema.registry.filter((trait) => !rows.some((row) => row.key === trait.key));
  const traitOf = (key: string) => schema.registry.find((trait) => trait.key === key);

  function change(index: number, patch: Partial<Row>) {
    setRows((current) => current.map((row, at) => (at === index ? { ...row, ...patch } : row)));
  }

  function move(index: number, by: -1 | 1) {
    setRows((current) => {
      const target = index + by;
      const next = [...current];
      const [row] = next.splice(index, 1);
      if (row === undefined || target < 0 || target > next.length) return current;
      next.splice(target, 0, row);
      return next;
    });
  }

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    setNotice(null);
    setProblems({});
    try {
      await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/contact-definition", {
          params: { path: { ws: workspaceId } },
          body: { fields: rows.map(toField) },
        }),
      );
      setNotice("Saved.");
      onSaved();
    } catch (error) {
      if (error instanceof ApiError && error.errors.length > 0) {
        setProblems(Object.fromEntries(error.errors.map((entry) => [entry.index, reasonText(entry.reason)])));
      } else {
        setFailure(describeError(error));
      }
    } finally {
      setBusy(false);
    }
  }

  return (
    <form className="stack" onSubmit={(event) => void submit(event)} noValidate aria-label="Contact definition">
      {!schema.defined ? (
        <p className="muted">
          No definition has been saved yet, so contacts carry only a key. Pick the fields below and save.
        </p>
      ) : null}
      <Panel title="Fields">
        {rows.length === 0 ? <p className="muted">No fields yet. A contact is only its key.</p> : null}
        <ol className="stack" aria-label="Fields in order">
          {rows.map((row, index) => (
            <li key={row.key} className="row-card">
              <div className="row-grow stack-tight">
                <span className="strong mono">{row.key}</span>
                <span className="faint">{typeName(traitOf(row.key))}</span>
                {problems[index] ? (
                  <span className="field-error" role="alert">
                    {problems[index]}
                  </span>
                ) : null}
              </div>
              <label className="check">
                <input
                  type="checkbox"
                  checked={row.required}
                  onChange={(event) => change(index, { required: event.target.checked })}
                />
                <span>
                  Required<span className="visually-hidden"> {row.key}</span>
                </span>
              </label>
              <SelectField
                label={`Identifier for ${row.key}`}
                value={row.kind}
                onChange={(event) => change(index, { kind: event.target.value })}
              >
                <option value="">Not an identifier</option>
                {IDENTIFIER_KINDS.map((kind) => (
                  <option key={kind.value} value={kind.value}>
                    {kind.label}
                  </option>
                ))}
              </SelectField>
              {row.kind === "external" ? (
                <TextField
                  label={`System for ${row.key}`}
                  mono
                  hint="Lowercase letters, digits, dots, hyphens and underscores."
                  value={row.system}
                  onChange={(event) => change(index, { system: event.target.value })}
                  autoComplete="off"
                />
              ) : null}
              <div className="row">
                <Button onClick={() => move(index, -1)} disabled={index === 0} aria-label={`Move ${row.key} up`}>
                  Up
                </Button>
                <Button
                  onClick={() => move(index, 1)}
                  disabled={index === rows.length - 1}
                  aria-label={`Move ${row.key} down`}
                >
                  Down
                </Button>
                <Button
                  onClick={() => setRows((current) => current.filter((_, at) => at !== index))}
                  aria-label={`Remove ${row.key}`}
                >
                  Remove
                </Button>
              </div>
            </li>
          ))}
        </ol>
        <div className="inline-form">
          <SelectField
            label="Add a field"
            hint={free.length === 0 ? "Every field of the registry is already in the definition." : undefined}
            value={adding}
            onChange={(event) => setAdding(event.target.value)}
          >
            <option value="">Pick a field</option>
            {free.map((trait) => (
              <option key={trait.key} value={trait.key}>
                {trait.key} ({trait.type})
              </option>
            ))}
          </SelectField>
          <div>
            <Button
              disabled={adding === ""}
              onClick={() => {
                setRows((current) => [...current, { key: adding, required: false, kind: "", system: "" }]);
                setAdding("");
              }}
            >
              Add to definition
            </Button>
          </div>
        </div>
        <p className="faint">
          The contact key is always there and always required. Add fields to the registry first, on the{" "}
          <Link to={`/w/${workspaceId}/traits`}>Fields</Link> page.
        </p>
      </Panel>
      {failure ? <InlineError {...failure} /> : null}
      <div className="row">
        <Button type="submit" variant="primary" disabled={busy}>
          {busy ? "Saving" : "Save definition"}
        </Button>
        {notice ? (
          <Pill>
            <span role="status">{notice}</span>
          </Pill>
        ) : null}
      </div>
    </form>
  );
}
