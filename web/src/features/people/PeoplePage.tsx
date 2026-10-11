import { useState, type SyntheticEvent } from "react";
import { Link, useParams } from "react-router";

import { ApiError, client, describeError, unwrap, useResource } from "../../api";
import {
  Button,
  EmptyState,
  ErrorState,
  formatDateTime,
  InlineError,
  kindLabel,
  PageHeader,
  Panel,
  ResourceView,
  SelectField,
  shortId,
  TextField,
} from "../../ui";
import { HiddenValue } from "./HiddenValue";
import { typeLabel } from "./TraitDialog";
import { operatorsFor, parseSearchValue, valueHint } from "./traitSearch";
import { useSearch, type PersonHit, type SearchQuery } from "./useSearch";

const IDENTIFIER_KINDS = [
  { value: "email", label: "Email address" },
  { value: "phone", label: "Phone number" },
  { value: "anonymous_id", label: "Anonymous ID" },
  { value: "external", label: "External ID" },
];

type Mode = "identifier" | "trait";

export function PeoplePage() {
  const { ws = "" } = useParams();
  return <People key={ws} workspaceId={ws} />;
}

function People({ workspaceId }: { workspaceId: string }) {
  const [query, setQuery] = useState<SearchQuery | null>(null);
  const search = useSearch(workspaceId, query);

  function run(request: SearchQuery["request"]) {
    setQuery((current) => ({ run: (current?.run ?? 0) + 1, request }));
  }

  return (
    <div className="page">
      <PageHeader
        title="People"
        lede="Find a person by an identifier, or by the value of one of your fields, and open everything the system knows and did about them."
      />
      <SearchForm workspaceId={workspaceId} onSearch={run} />
      <Results workspaceId={workspaceId} searched={query !== null} search={search} />
    </div>
  );
}

function SearchForm({ workspaceId, onSearch }: { workspaceId: string; onSearch: (request: SearchQuery["request"]) => void }) {
  const [mode, setMode] = useState<Mode>("identifier");

  return (
    <Panel>
      <div className="row" role="group" aria-label="Search by">
        <Button variant={mode === "identifier" ? "primary" : "quiet"} aria-pressed={mode === "identifier"} onClick={() => setMode("identifier")}>
          Identifier
        </Button>
        <Button variant={mode === "trait" ? "primary" : "quiet"} aria-pressed={mode === "trait"} onClick={() => setMode("trait")}>
          Field value
        </Button>
      </div>
      {mode === "identifier" ? (
        <IdentifierForm onSearch={onSearch} />
      ) : (
        <TraitForm workspaceId={workspaceId} onSearch={onSearch} />
      )}
    </Panel>
  );
}

function IdentifierForm({ onSearch }: { onSearch: (request: SearchQuery["request"]) => void }) {
  const [kind, setKind] = useState("email");
  const [namespace, setNamespace] = useState("");
  const [value, setValue] = useState("");
  const [problem, setProblem] = useState<string | null>(null);

  function submit(event: SyntheticEvent) {
    event.preventDefault();
    if (kind === "external" && namespace.trim() === "") {
      setProblem("Enter the name of the system the ID comes from.");
      return;
    }
    if (value.trim() === "") {
      setProblem("Enter a value to search for.");
      return;
    }
    setProblem(null);
    onSearch({
      identifier: { kind: kind === "external" ? `external:${namespace.trim()}` : kind, value: value.trim() },
    });
  }

  return (
    <form className="stack" onSubmit={submit} aria-label="Search by identifier">
      <div className="row">
        <SelectField label="Kind" value={kind} onChange={(event) => setKind(event.target.value)}>
          {IDENTIFIER_KINDS.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </SelectField>
        {kind === "external" ? (
          <TextField
            label="System"
            mono
            hint="Lowercase letters, digits, dots, hyphens and underscores."
            value={namespace}
            onChange={(event) => setNamespace(event.target.value)}
            autoComplete="off"
          />
        ) : null}
        <TextField
          label="Value"
          hint="Matched exactly, after the usual clean-up of addresses and numbers."
          value={value}
          onChange={(event) => setValue(event.target.value)}
          autoComplete="off"
        />
      </div>
      {problem ? <InlineError title={problem} /> : null}
      <div>
        <Button type="submit" variant="primary">
          Search
        </Button>
      </div>
    </form>
  );
}

function TraitForm({ workspaceId, onSearch }: { workspaceId: string; onSearch: (request: SearchQuery["request"]) => void }) {
  const traits = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/traits", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [key, setKey] = useState("");
  const [operator, setOperator] = useState("eq");
  const [value, setValue] = useState("");
  const [problem, setProblem] = useState<string | null>(null);

  if (traits.status === "loading") {
    return <p role="status">Loading your fields</p>;
  }
  if (traits.status === "error") {
    return <ErrorState {...describeError(traits.error)} onRetry={traits.reload} />;
  }
  const choices = traits.data.filter((trait) => !trait.archived);
  if (choices.length === 0) {
    return (
      <EmptyState title="No fields to search by yet">
        Fields appear once a source sends a value for them, or you add one on the Fields page.
      </EmptyState>
    );
  }
  const chosen = choices.find((trait) => trait.key === key) ?? choices[0];
  if (!chosen) {
    return null;
  }
  const { key: chosenKey, type } = chosen;
  const operators = operatorsFor(type);
  const active = operators.some((option) => option.value === operator) ? operator : "eq";

  function submit(event: SyntheticEvent) {
    event.preventDefault();
    const parsed = parseSearchValue(type, active, value);
    if (!parsed.ok) {
      setProblem(parsed.message);
      return;
    }
    setProblem(null);
    onSearch({ trait: { key: chosenKey, op: active, value: parsed.value } });
  }

  return (
    <form className="stack" onSubmit={submit} aria-label="Search by field value">
      <div className="row">
        <SelectField
          label="Field"
          value={chosenKey}
          onChange={(event) => {
            setKey(event.target.value);
            setOperator("eq");
            setValue("");
          }}
        >
          {choices.map((trait) => (
            <option key={trait.key} value={trait.key}>
              {trait.key} ({typeLabel(trait.type)})
            </option>
          ))}
        </SelectField>
        <SelectField label="Condition" value={active} onChange={(event) => setOperator(event.target.value)}>
          {operators.map((option) => (
            <option key={option.value} value={option.value}>
              {option.label}
            </option>
          ))}
        </SelectField>
        {active === "exists" ? null : type === "boolean" ? (
          <SelectField label="Value" value={value} onChange={(event) => setValue(event.target.value)}>
            <option value="">Choose</option>
            <option value="true">Yes</option>
            <option value="false">No</option>
          </SelectField>
        ) : (
          <TextField
            label="Value"
            hint={valueHint(type)}
            value={value}
            onChange={(event) => setValue(event.target.value)}
            autoComplete="off"
          />
        )}
      </div>
      {problem ? <InlineError title={problem} /> : null}
      <div>
        <Button type="submit" variant="primary">
          Search
        </Button>
      </div>
    </form>
  );
}

function failureOf(error: unknown) {
  if (error instanceof ApiError && error.code === "search_timeout") {
    return {
      title: "That search took too long",
      detail: "Narrow it down, for example with an identifier or a more specific value, and try again.",
      traceId: error.traceId,
    };
  }
  return describeError(error);
}

function Results({ workspaceId, searched, search }: { workspaceId: string; searched: boolean; search: ReturnType<typeof useSearch> }) {
  if (!searched) {
    return (
      <Panel flush>
        <EmptyState title="Search to find people">Use an exact identifier or a field value above.</EmptyState>
      </Panel>
    );
  }
  const { resource } = search;
  return (
    <Panel flush>
      {resource.status === "error" ? (
        <ErrorState {...failureOf(resource.error)} onRetry={resource.reload} />
      ) : (
        <ResourceView
          resource={resource}
          loadingLabel="Searching"
          isEmpty={(data) => data.length === 0}
          empty={<EmptyState title="No people match">Check the spelling, or try another identifier or value.</EmptyState>}
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>People that match, ordered by person</caption>
                <thead>
                  <tr>
                    <th scope="col">Identifier</th>
                    <th scope="col">Updated</th>
                    <th scope="col">Person</th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((person) => (
                    <PersonRow key={person.id} workspaceId={workspaceId} person={person} />
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
      )}
      {resource.status === "ready" && search.nextAfter !== null ? (
        <div className="panel-footer">
          {search.moreError ? <InlineError {...failureOf(search.moreError)} /> : null}
          <Button onClick={search.loadMore} disabled={search.loadingMore}>
            {search.loadingMore ? "Loading" : "Show more"}
          </Button>
        </div>
      ) : null}
    </Panel>
  );
}

function PersonRow({ workspaceId, person }: { workspaceId: string; person: PersonHit }) {
  return (
    <tr>
      <td>
        {person.display ? (
          <>
            <div className="strong">{person.display.masked ? <HiddenValue /> : person.display.value}</div>
            <div className="faint">{kindLabel(person.display.kind)}</div>
          </>
        ) : (
          <span className="faint">No identifier</span>
        )}
      </td>
      <td>{formatDateTime(person.updated_at)}</td>
      <td>
        <Link to={`/w/${workspaceId}/people/${person.id}`}>
          Open <span className="mono">{shortId(person.id)}</span>
        </Link>
      </td>
    </tr>
  );
}
