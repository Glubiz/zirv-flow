import { useState, type SyntheticEvent } from "react";
import { Link, useParams } from "react-router";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import {
  Button,
  Dialog,
  EmptyState,
  InlineError,
  PageHeader,
  Panel,
  Pill,
  ResourceView,
  SelectField,
  TextField,
  formatDateTime,
} from "../../ui";
import { useWorkspaceAccess } from "../members";
import { EventsNav } from "./EventsNav";
import { MODES, modeLabel } from "./labels";

export function EventsPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);
  const canManage = access.status === "ready" && access.data.canAdmin;

  return (
    <div className="page">
      <PageHeader
        title="Events"
        lede="What people did, as typed events. Each event name has a schema that says which properties it carries, and what happens to an event that breaks it."
      />
      <EventsNav workspaceId={ws} />
      <SchemaList key={ws} workspaceId={ws} canManage={canManage} />
    </div>
  );
}

function SchemaList({ workspaceId, canManage }: { workspaceId: string; canManage: boolean }) {
  const schemas = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/event-schemas", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const [adding, setAdding] = useState(false);

  return (
    <>
      {canManage ? (
        <div className="page-actions">
          <Button variant="primary" onClick={() => setAdding(true)}>
            Add schema
          </Button>
        </div>
      ) : null}
      <Panel flush>
        <ResourceView
          resource={schemas}
          loadingLabel="Loading schemas"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState title="No event schemas yet">
              A schema is drafted automatically the first time an unknown event arrives
              {canManage ? ", or you can add one yourself to choose its properties up front." : "."}
            </EmptyState>
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Event schemas of this workspace</caption>
                <thead>
                  <tr>
                    <th scope="col">Event</th>
                    <th scope="col">Mode</th>
                    <th scope="col">Status</th>
                    <th scope="col">Version</th>
                    <th scope="col">Updated</th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((schema) => (
                    <tr key={schema.name}>
                      <td className="strong">
                        <Link to={`/w/${workspaceId}/events/schemas/${encodeURIComponent(schema.name)}`}>{schema.name}</Link>
                        {schema.description ? <div className="faint">{schema.description}</div> : null}
                      </td>
                      <td>{modeLabel(schema.mode)}</td>
                      <td>
                        {schema.current_version === null || schema.current_version === undefined ? (
                          <Pill tone="attention">Not published</Pill>
                        ) : (
                          <Pill>Published</Pill>
                        )}
                        {schema.has_draft ? <Pill>Draft</Pill> : null}
                        {schema.inferred ? <Pill>Inferred</Pill> : null}
                      </td>
                      <td>{schema.current_version ?? "—"}</td>
                      <td>{formatDateTime(schema.updated_at)}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
      </Panel>
      {!canManage ? <p className="muted">Only workspace admins and the instance owner can add or change schemas.</p> : null}
      {adding ? (
        <CreateSchemaDialog
          workspaceId={workspaceId}
          onClose={() => setAdding(false)}
          onSaved={() => {
            setAdding(false);
            schemas.reload();
          }}
        />
      ) : null}
    </>
  );
}

interface CreateSchemaDialogProps {
  workspaceId: string;
  onClose: () => void;
  onSaved: () => void;
}

function CreateSchemaDialog({ workspaceId, onClose, onSaved }: CreateSchemaDialogProps) {
  const [name, setName] = useState("");
  const [mode, setMode] = useState("warn");
  const [description, setDescription] = useState("");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(
        client.POST("/api/v1/workspaces/{ws}/event-schemas", {
          params: { path: { ws: workspaceId } },
          body: { name: name.trim(), mode, description, properties: [] },
        }),
      );
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <Dialog title="Add an event schema" onClose={onClose}>
      <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
        <TextField
          label="Event name"
          hint="Up to 128 characters. It cannot be renamed. You add the properties on the next page."
          value={name}
          onChange={(event) => setName(event.target.value)}
          autoComplete="off"
          required
        />
        <SelectField
          label="When an event breaks the schema"
          hint={MODES.find((option) => option.value === mode)?.help}
          value={mode}
          onChange={(event) => setMode(event.target.value)}
        >
          {MODES.map((option) => (
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
          <Button type="submit" variant="primary" disabled={busy || name.trim() === ""}>
            {busy ? "Adding" : "Add schema"}
          </Button>
        </div>
      </form>
    </Dialog>
  );
}
