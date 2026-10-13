import { useState } from "react";

import { client, describeError, unwrap, useResource, type components, type ErrorSummary } from "../../api";
import { Button, ErrorState, InlineError, LoadingState, SelectField, TransformEditor } from "../../ui";
import {
  draftProblems,
  draftsToMapping,
  mappingToDrafts,
  type ColumnDraft,
  type DraftSet,
  type IdentifierChoice,
  type TargetChoice,
} from "./mapping";

type Trait = components["schemas"]["TraitBody"];
type ListItem = components["schemas"]["ListBody"];
type Detail = components["schemas"]["CsvImportDetailBody"];

const SAMPLE_VALUES = 3;

const IDENTIFIER_LABELS: Record<IdentifierChoice, string> = {
  email: "Email address",
  phone: "Phone number",
  anonymous_id: "Anonymous ID",
  external: "External ID…",
};

/** The `value` of the option a choice stands for. */
export function choiceValue(choice: TargetChoice): string {
  switch (choice.kind) {
    case "ignore":
      return "ignore";
    case "identifier":
      return `identifier:${choice.identifier}`;
    case "trait":
      return `trait:${choice.key}`;
    case "list":
      return `list:${choice.listId}`;
  }
}

export function choiceFrom(value: string, previous: TargetChoice): TargetChoice {
  if (value.startsWith("identifier:")) {
    const identifier = value.slice("identifier:".length) as IdentifierChoice;
    return {
      kind: "identifier",
      identifier,
      namespace: previous.kind === "identifier" ? previous.namespace : "",
    };
  }
  if (value.startsWith("trait:")) return { kind: "trait", key: value.slice("trait:".length) };
  if (value.startsWith("list:")) return { kind: "list", listId: value.slice("list:".length) };
  return { kind: "ignore" };
}

interface MappingStepProps {
  workspaceId: string;
  importId: string;
  canWrite: boolean;
  /** Called once the mapping is saved; `dry-run` when the dry run was started too. */
  onSaved: (next: "map" | "dry-run") => void;
}

export function MappingStep({ workspaceId, importId, canWrite, onSaved }: MappingStepProps) {
  const detail = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/imports/{id}", {
          params: { path: { ws: workspaceId, id: importId }, query: { samples: true } },
        }),
      ),
    [workspaceId, importId],
  );
  const traits = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/traits", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const lists = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/lists", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );

  if (detail.status === "ready" && traits.status === "ready" && lists.status === "ready") {
    return (
      <MappingForm
        workspaceId={workspaceId}
        importId={importId}
        canWrite={canWrite}
        detail={detail.data}
        traits={traits.data.filter((item) => !item.archived)}
        lists={lists.data}
        onSaved={onSaved}
      />
    );
  }
  const failed = [detail, traits, lists].find((resource) => resource.status === "error");
  if (failed?.status === "error") {
    return <ErrorState {...describeError(failed.error)} onRetry={failed.reload} />;
  }
  return <LoadingState label="Loading the file and your fields" />;
}

interface MappingFormProps {
  workspaceId: string;
  importId: string;
  canWrite: boolean;
  detail: Detail;
  traits: Trait[];
  lists: ListItem[];
  onSaved: (next: "map" | "dry-run") => void;
}

function sampleText(detail: Detail, index: number): string {
  const values = detail.samples
    .map((row) => row[index])
    .filter((value): value is string => typeof value === "string" && value !== "")
    .slice(0, SAMPLE_VALUES);
  return values.join(" · ");
}

function MappingForm({ workspaceId, importId, canWrite, detail, traits, lists, onSaved }: MappingFormProps) {
  const [initial] = useState<DraftSet>(() => mappingToDrafts(detail.header, detail.mapping));
  const [drafts, setDrafts] = useState<ColumnDraft[]>(initial.drafts);
  const [keyColumn, setKeyColumn] = useState(initial.keyColumn);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [saved, setSaved] = useState(false);

  const problems = draftProblems(drafts);
  const disabled = !canWrite || busy;

  function change(index: number, next: Partial<ColumnDraft>) {
    setSaved(false);
    setDrafts((current) => current.map((draft, at) => (at === index ? { ...draft, ...next } : draft)));
  }

  async function save(thenDryRun: boolean) {
    setBusy(true);
    setFailure(null);
    try {
      const path = { ws: workspaceId, id: importId };
      await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/imports/{id}/mapping", {
          params: { path },
          body: draftsToMapping(drafts, keyColumn),
        }),
      );
      if (thenDryRun) {
        await unwrap(client.POST("/api/v1/workspaces/{ws}/imports/{id}/dry-run", { params: { path } }));
      }
      setSaved(true);
      onSaved(thenDryRun ? "dry-run" : "map");
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="stack">
      <p className="muted">
        Choose what each column becomes. A column can identify a person (an email address, a phone number or an ID), fill one
        of your fields, add the person to a list when the cell says yes, or be left out. Steps change a cell on its way,
        for example lowercasing an email address or reading a date.
      </p>
      {detail.samples_masked ? (
        <p className="notice">Sample values are hidden for your role. The mapping works the same.</p>
      ) : null}
      {initial.repeated.length > 0 ? (
        <p className="notice" role="status">
          This mapping has more than one rule for {initial.repeated.join(", ")}. Only the first is shown here; saving keeps one rule
          per column.
        </p>
      ) : null}
      <SelectField
        label="Key column"
        value={keyColumn}
        onChange={(event) => {
          setSaved(false);
          setKeyColumn(event.target.value);
        }}
        disabled={disabled}
        hint="A column with a value that is unique for each person, such as a customer number. Running a changed file again then updates the same records. Without one, only the very same file can be run again without changes."
      >
        <option value="">No key column (use the file and the row number)</option>
        {detail.header.map((name) => (
          <option key={name} value={name}>
            {name}
          </option>
        ))}
      </SelectField>
      <div className="table-scroll">
        <table className="table">
          <caption>Columns of the file</caption>
          <thead>
            <tr>
              <th scope="col">Column</th>
              <th scope="col">First values</th>
              <th scope="col">Becomes</th>
              <th scope="col">Steps</th>
            </tr>
          </thead>
          <tbody>
            {drafts.map((draft, index) => {
              const value = choiceValue(draft.target);
              const traitKey = draft.target.kind === "trait" ? draft.target.key : null;
              const known = traitKey === null || traits.some((item) => item.key === traitKey);
              return (
                <tr key={draft.column}>
                  <th scope="row" className="strong">
                    {draft.column}
                  </th>
                  <td className="faint">{detail.samples_masked ? "Hidden" : sampleText(detail, index) || "—"}</td>
                  <td>
                    <div className="stack-tight">
                      <select
                        className="select"
                        aria-label={`What ${draft.column} becomes`}
                        value={value}
                        disabled={disabled}
                        onChange={(event) => change(index, { target: choiceFrom(event.target.value, draft.target) })}
                      >
                        <option value="ignore">Leave out</option>
                        <optgroup label="Identifies a person">
                          {(Object.keys(IDENTIFIER_LABELS) as IdentifierChoice[]).map((kind) => (
                            <option key={kind} value={`identifier:${kind}`}>
                              {IDENTIFIER_LABELS[kind]}
                            </option>
                          ))}
                        </optgroup>
                        <optgroup label="Fills a field">
                          {traits.map((item) => (
                            <option key={item.key} value={`trait:${item.key}`}>
                              {item.key} ({item.type})
                            </option>
                          ))}
                          {known ? null : <option value={value}>{value.slice("trait:".length)} (not available)</option>}
                        </optgroup>
                        <optgroup label="Adds to a list when the cell is yes">
                          {lists.map((item) => (
                            <option key={item.id} value={`list:${item.id}`}>
                              {item.name}
                            </option>
                          ))}
                        </optgroup>
                      </select>
                      {draft.target.kind === "identifier" && draft.target.identifier === "external" ? (
                        <input
                          className="input input-mono"
                          aria-label={`Name of the external ID in ${draft.column}`}
                          placeholder="crm"
                          value={draft.target.namespace}
                          disabled={disabled}
                          onChange={(event) =>
                            change(index, {
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
                        Steps ({draft.transforms.length})
                        <span className="visually-hidden"> for {draft.column}</span>
                      </summary>
                      <TransformEditor
                        column={draft.column}
                        others={detail.header.filter((name) => name !== draft.column)}
                        steps={draft.transforms}
                        disabled={disabled}
                        onChange={(steps) => change(index, { transforms: steps })}
                      />
                    </details>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {problems.length > 0 ? (
        <ul className="list-plain attention" aria-label="Before you can continue">
          {problems.map((problem) => (
            <li key={problem}>{problem}</li>
          ))}
        </ul>
      ) : null}
      {failure ? <InlineError {...failure} /> : null}
      {saved ? (
        <p className="notice" role="status">
          Mapping saved.
        </p>
      ) : null}
      {canWrite ? (
        <div className="row">
          <Button variant="primary" onClick={() => void save(true)} disabled={busy || problems.length > 0}>
            {busy ? "Saving" : "Save and dry run"}
          </Button>
          <Button onClick={() => void save(false)} disabled={busy || problems.length > 0}>
            Save mapping
          </Button>
        </div>
      ) : (
        <p className="muted">You can look at this import but not change it.</p>
      )}
    </div>
  );
}
