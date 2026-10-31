import { useState, type SyntheticEvent } from "react";
import { useParams } from "react-router";

import { client, describeError, unwrap, useResource, type ErrorSummary } from "../../api";
import {
  Button,
  Dialog,
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
import { PropertyEditor, PropertyTable, rowFrom, rowToProperty, type PropertyRow } from "./PropertyEditor";
import { MODES, modeLabel, type EventSchemaDetail } from "./labels";

export function EventSchemaPage() {
  const { ws = "", name = "" } = useParams();
  const access = useWorkspaceAccess(ws);
  const canManage = access.status === "ready" && access.data.canAdmin;

  return (
    <div className="page">
      <PageHeader
        title={name}
        breadcrumb={{ group: "Data", section: "Events", to: `/w/${ws}/events` }}
        lede="Its mode, retention, draft and published versions."
      />
      <EventsNav workspaceId={ws} />
      <SchemaView key={`${ws}/${name}`} workspaceId={ws} name={name} canManage={canManage} />
    </div>
  );
}

interface SchemaViewProps {
  workspaceId: string;
  name: string;
  canManage: boolean;
}

function SchemaView({ workspaceId, name, canManage }: SchemaViewProps) {
  const detail = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/event-schemas/{name}", {
          params: { path: { ws: workspaceId, name } },
        }),
      ),
    [workspaceId, name],
  );

  return (
    <ResourceView resource={detail} loadingLabel="Loading schema">
      {(data) => (
        <SchemaBody
          key={data.schema.updated_at}
          workspaceId={workspaceId}
          data={data}
          canManage={canManage}
          onChanged={detail.reload}
        />
      )}
    </ResourceView>
  );
}

interface SchemaBodyProps {
  workspaceId: string;
  data: EventSchemaDetail;
  canManage: boolean;
  onChanged: () => void;
}

function SchemaBody({ workspaceId, data, canManage, onChanged }: SchemaBodyProps) {
  return (
    <>
      {!canManage ? <p className="muted">Only workspace admins and the instance owner can change this schema.</p> : null}
      <Panel title="Settings">
        {canManage ? (
          <SettingsForm workspaceId={workspaceId} data={data} onSaved={onChanged} />
        ) : (
          <dl className="facts">
            <dt>Mode</dt>
            <dd>{modeLabel(data.schema.mode)}</dd>
            <dt>Retention</dt>
            <dd>{retentionText(data.schema.retention_days)}</dd>
          </dl>
        )}
      </Panel>
      <Panel title="Draft">
        <DraftSection workspaceId={workspaceId} data={data} canManage={canManage} onChanged={onChanged} />
      </Panel>
      <Panel title="Published versions">
        {data.versions.length === 0 ? (
          <p className="muted">Nothing is published. Until a version is published, events of this name follow the workspace policy for unplanned events.</p>
        ) : (
          <div className="stack">
            {data.versions.map((version) => (
              <details key={version.version} className="version" open={version.version === data.schema.current_version}>
                <summary>
                  Version {version.version}
                  <span className="faint"> published {formatDateTime(version.published_at)}</span>
                  {version.version === data.schema.current_version ? <Pill>In force</Pill> : null}
                </summary>
                <PropertyTable properties={version.properties} caption={`Properties of version ${version.version}`} />
              </details>
            ))}
          </div>
        )}
      </Panel>
    </>
  );
}

function retentionText(days: number | null | undefined): string {
  return days === null || days === undefined ? "The deployment default" : `${days} days`;
}

interface SettingsFormProps {
  workspaceId: string;
  data: EventSchemaDetail;
  onSaved: () => void;
}

function SettingsForm({ workspaceId, data, onSaved }: SettingsFormProps) {
  const schema = data.schema;
  const [mode, setMode] = useState(schema.mode);
  const [retention, setRetention] = useState(schema.retention_days === null || schema.retention_days === undefined ? "" : String(schema.retention_days));
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const retentionValue = retention.trim() === "" ? null : Number(retention);
  const modeChanged = mode !== schema.mode;
  const retentionChanged = retentionValue !== (schema.retention_days ?? null);
  const invalid = retentionValue !== null && !Number.isInteger(retentionValue);

  async function submit(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(
        client.PATCH("/api/v1/workspaces/{ws}/event-schemas/{name}", {
          params: { path: { ws: workspaceId, name: schema.name } },
          body: {
            ...(modeChanged ? { mode } : {}),
            ...(retentionChanged ? { retention_days: retentionValue } : {}),
          },
        }),
      );
      onSaved();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form className="stack" onSubmit={(event) => void submit(event)} noValidate>
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
        label="Retention in days"
        type="number"
        inputMode="numeric"
        min={1}
        hint="How long events of this name are kept. Leave empty for the deployment default. The operator sets the allowed range."
        value={retention}
        onChange={(event) => setRetention(event.target.value)}
      />
      {failure ? <InlineError {...failure} /> : null}
      <div>
        <Button type="submit" variant="primary" disabled={busy || invalid || (!modeChanged && !retentionChanged)}>
          {busy ? "Saving" : "Save settings"}
        </Button>
      </div>
    </form>
  );
}

interface DraftSectionProps {
  workspaceId: string;
  data: EventSchemaDetail;
  canManage: boolean;
  onChanged: () => void;
}

function DraftSection({ workspaceId, data, canManage, onChanged }: DraftSectionProps) {
  const latest = data.versions[0];
  const initial = (data.draft ?? []).map(rowFrom);
  const [rows, setRows] = useState<PropertyRow[]>(initial);
  const [started, setStarted] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  const hasDraft = data.draft !== null && data.draft !== undefined;
  const editing = hasDraft || started;
  const dirty = started || JSON.stringify(rows) !== JSON.stringify(initial);

  if (!canManage) {
    return hasDraft ? (
      <PropertyTable properties={data.draft ?? []} caption="Properties of the draft" />
    ) : (
      <p className="muted">There is no draft.</p>
    );
  }

  async function save() {
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(
        client.PUT("/api/v1/workspaces/{ws}/event-schemas/{name}/draft", {
          params: { path: { ws: workspaceId, name: data.schema.name } },
          body: { properties: rows.map(rowToProperty) },
        }),
      );
      onChanged();
    } catch (error) {
      setFailure(describeError(error));
    } finally {
      setBusy(false);
    }
  }

  async function publish() {
    setBusy(true);
    setFailure(null);
    try {
      await unwrap(
        client.POST("/api/v1/workspaces/{ws}/event-schemas/{name}/versions", {
          params: { path: { ws: workspaceId, name: data.schema.name } },
        }),
      );
      setConfirming(false);
      onChanged();
    } catch (error) {
      setConfirming(false);
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  if (!editing) {
    return (
      <div className="stack">
        <p className="muted">
          There is no draft.
          {latest ? ` Version ${latest.version} is in force.` : ""} Start a draft to change the properties; nothing changes
          for incoming events until you publish it.
        </p>
        <div>
          <Button
            onClick={() => {
              setRows((latest?.properties ?? []).map(rowFrom));
              setStarted(true);
            }}
          >
            {latest ? `Start a draft from version ${latest.version}` : "Start a draft"}
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="stack">
      {data.schema.inferred ? (
        <p className="muted">
          This draft was inferred from events that arrived without a schema. Review it, then publish it to put it in force.
        </p>
      ) : null}
      <PropertyEditor rows={rows} onChange={setRows} />
      {failure ? <InlineError {...failure} /> : null}
      <div className="row">
        <Button variant="primary" onClick={() => void save()} disabled={busy || !dirty}>
          {busy ? "Saving" : "Save draft"}
        </Button>
        <Button onClick={() => setConfirming(true)} disabled={busy || dirty || !hasDraft}>
          Publish
        </Button>
        {dirty ? <span className="faint">Save the draft before you publish it.</span> : null}
      </div>
      {confirming ? (
        <Dialog title="Publish this draft?" onClose={() => setConfirming(false)}>
          <p className="muted">
            The draft becomes version {(data.schema.current_version ?? 0) + 1} and takes effect for new events straight away.
            A published version can never be changed or removed.
          </p>
          <div className="dialog-actions">
            <Button onClick={() => setConfirming(false)}>Keep editing</Button>
            <Button variant="primary" onClick={() => void publish()} disabled={busy}>
              {busy ? "Publishing" : "Publish version"}
            </Button>
          </div>
        </Dialog>
      ) : null}
    </div>
  );
}
