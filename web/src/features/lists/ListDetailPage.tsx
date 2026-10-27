import { useState, type SyntheticEvent } from "react";
import { Link, useParams } from "react-router";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
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
import { plural, sourceLabel, useListAccess } from "./access";
import { ImportDialog } from "./ImportDialog";
import { useListMembers, type ListMember } from "./useListMembers";

const KINDS = ["email", "phone", "anonymous_id", "external"];

export function ListDetailPage() {
  const { ws = "", listId = "" } = useParams();
  return <ListDetail key={`${ws}/${listId}`} workspaceId={ws} listId={listId} />;
}

function memberLabel(member: ListMember): string {
  return member.identifier?.value ?? `person ${shortId(member.person_id)}`;
}

function ListDetail({ workspaceId, listId }: { workspaceId: string; listId: string }) {
  const { canWrite, canSeeValues } = useListAccess(workspaceId);
  const list = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/lists/{id}", { params: { path: { ws: workspaceId, id: listId } } }),
      ),
    [workspaceId, listId],
  );
  const members = useListMembers(workspaceId, listId);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [importing, setImporting] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [busy, setBusy] = useState(false);

  function refresh() {
    setSelected(new Set());
    list.reload();
    members.resource.reload();
  }

  async function removeSelected() {
    setBusy(true);
    setFailure(null);
    setNotice(null);
    try {
      const result = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/lists/{id}/members/remove", {
          params: { path: { ws: workspaceId, id: listId } },
          body: { person_ids: [...selected] },
        }),
      );
      setNotice(`Removed ${plural(result.removed, "person", "people")}.`);
      refresh();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  function toggle(personId: string) {
    setSelected((current) => {
      const next = new Set(current);
      if (!next.delete(personId)) next.add(personId);
      return next;
    });
  }

  const title = list.status === "ready" ? list.data.name : "List";
  const lede = list.status === "ready" ? list.data.description : undefined;

  return (
    <div className="page">
      <p>
        <Link to={`/w/${workspaceId}/lists`}>All lists</Link>
      </p>
      {list.status === "error" ? (
        <Panel flush>
          <ErrorState {...describeError(list.error)} onRetry={list.reload} />
        </Panel>
      ) : (
        <>
          <PageHeader
            title={title}
            lede={lede || (list.status === "ready" ? `${plural(list.data.member_count, "person", "people")} in this list.` : undefined)}
            actions={
              canWrite ? (
                <Button onClick={() => setImporting(true)} disabled={list.status !== "ready"}>
                  Import CSV
                </Button>
              ) : null
            }
          />
          {list.status === "ready" && lede ? (
            <p className="muted">{plural(list.data.member_count, "person", "people")} in this list.</p>
          ) : null}
          {notice ? (
            <p className="notice" role="status">
              {notice}
            </p>
          ) : null}
          {canWrite ? <AddPerson workspaceId={workspaceId} listId={listId} onAdded={(message) => { setNotice(message); refresh(); }} /> : null}
          {failure ? <InlineError {...failure} /> : null}
          <Panel flush>
            <ResourceView
              resource={members.resource}
              loadingLabel="Loading members"
              isEmpty={(data) => data.length === 0}
              empty={
                <EmptyState title="No one is in this list yet">
                  {canWrite
                    ? "Add a person by identifier above, or import a CSV of identifiers."
                    : "People appear here once someone adds them."}
                </EmptyState>
              }
            >
              {(data) => (
                <div className="table-scroll">
                  <table className="table">
                    <caption>People in this list, newest first</caption>
                    <thead>
                      <tr>
                        {canWrite ? (
                          <th scope="col">
                            <input
                              type="checkbox"
                              aria-label="Select everyone on this page"
                              checked={data.length > 0 && data.every((member) => selected.has(member.person_id))}
                              onChange={(event) =>
                                setSelected(event.target.checked ? new Set(data.map((member) => member.person_id)) : new Set())
                              }
                            />
                          </th>
                        ) : null}
                        <th scope="col">Identifier</th>
                        <th scope="col">Person</th>
                        <th scope="col">Added</th>
                        <th scope="col">How</th>
                      </tr>
                    </thead>
                    <tbody>
                      {data.map((member) => (
                        <tr key={member.person_id}>
                          {canWrite ? (
                            <td>
                              <input
                                type="checkbox"
                                aria-label={`Select ${memberLabel(member)}`}
                                checked={selected.has(member.person_id)}
                                onChange={() => toggle(member.person_id)}
                              />
                            </td>
                          ) : null}
                          <td>
                            {member.identifier ? (
                              <>
                                <div className="strong">
                                  {member.identifier.masked ? <span className="faint">Hidden</span> : member.identifier.value}
                                </div>
                                <div className="faint">{kindLabel(member.identifier.kind)}</div>
                              </>
                            ) : (
                              <span className="faint">No identifier</span>
                            )}
                          </td>
                          <td className="mono">{shortId(member.person_id)}</td>
                          <td>{formatDateTime(member.added_at)}</td>
                          <td>{sourceLabel(member.source)}</td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
            </ResourceView>
            {members.resource.status === "ready" && members.nextBefore !== null ? (
              <div className="panel-footer">
                {members.moreError ? <InlineError {...describeError(members.moreError)} /> : null}
                <Button onClick={members.loadMore} disabled={members.loadingMore}>
                  {members.loadingMore ? "Loading" : "Show more"}
                </Button>
              </div>
            ) : null}
            {canWrite && selected.size > 0 ? (
              <div className="panel-footer">
                <Button variant="primary" onClick={() => void removeSelected()} disabled={busy}>
                  {busy ? "Removing" : `Remove ${plural(selected.size, "selected person", "selected people")}`}
                </Button>
              </div>
            ) : null}
          </Panel>
          {!canWrite ? <p className="muted">You can look at this list but not change it.</p> : null}
          {!canSeeValues ? (
            <p className="muted">Identifier values are hidden without the personal data permission.</p>
          ) : null}
        </>
      )}
      {importing ? (
        <ImportDialog
          workspaceId={workspaceId}
          listId={listId}
          onClose={() => setImporting(false)}
          onImported={(result) => {
            setImporting(false);
            setNotice(
              `Added ${plural(result.added, "person", "people")}. ${result.already_member} already in the list, ${result.unmatched_count} skipped.`,
            );
            refresh();
          }}
        />
      ) : null}
    </div>
  );
}

interface AddPersonProps {
  workspaceId: string;
  listId: string;
  onAdded: (message: string) => void;
}

function AddPerson({ workspaceId, listId, onAdded }: AddPersonProps) {
  const [kind, setKind] = useState("email");
  const [namespace, setNamespace] = useState("");
  const [value, setValue] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);
  const [missing, setMissing] = useState(false);

  const fullKind = kind === "external" ? `external:${namespace.trim()}` : kind;
  const incomplete = value.trim() === "" || (kind === "external" && namespace.trim() === "");

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    setMissing(false);
    try {
      const result = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/lists/{id}/members", {
          params: { path: { ws: workspaceId, id: listId } },
          body: { identifiers: [{ kind: fullKind, value: value.trim() }] },
        }),
      );
      if (result.not_found.length > 0) {
        setMissing(true);
      } else {
        setValue("");
        onAdded(result.added > 0 ? "Added the person." : "That person is already in the list.");
      }
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <Panel title="Add a person">
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <div className="inline-form">
          <SelectField label="Identifier kind" value={kind} onChange={(event) => setKind(event.target.value)}>
            {KINDS.map((option) => (
              <option key={option} value={option}>
                {option === "external" ? "External ID" : kindLabel(option)}
              </option>
            ))}
          </SelectField>
          {kind === "external" ? (
            <TextField
              label="System name"
              hint="Lowercase, such as crm."
              value={namespace}
              onChange={(event) => setNamespace(event.target.value)}
              autoComplete="off"
            />
          ) : null}
          <TextField
            label="Identifier"
            value={value}
            onChange={(event) => setValue(event.target.value)}
            autoComplete="off"
            mono
          />
          <Button type="submit" variant="primary" disabled={busy || incomplete}>
            {busy ? "Adding" : "Add to list"}
          </Button>
        </div>
        {missing ? (
          <p className="form-error" role="alert">
            No one has that identifier. Only people who already exist can be added.
          </p>
        ) : null}
        {failure ? <InlineError {...failure} /> : null}
      </form>
    </Panel>
  );
}
