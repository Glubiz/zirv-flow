import { useState, type SyntheticEvent } from "react";
import { useNavigate, useParams } from "react-router";

import { client, describeError, unwrap, type ErrorSummary } from "../../api";
import { Button, EmptyState, InlineError, PageHeader, Panel, SelectField, TextField } from "../../ui";
import { useFlowAccess } from "./access";
import { defaultSettings, TRIGGER_KINDS, type TriggerType } from "./triggers";

export function NewFlowPage() {
  const { ws = "" } = useParams();
  return <NewFlow key={ws} workspaceId={ws} />;
}

function NewFlow({ workspaceId }: { workspaceId: string }) {
  const navigate = useNavigate();
  const { ready, canWrite } = useFlowAccess(workspaceId);
  const [name, setName] = useState("");
  const [trigger, setTrigger] = useState<TriggerType>("segment");
  const [busy, setBusy] = useState(false);
  const [failure, setFailure] = useState<ErrorSummary | null>(null);

  async function create(event: SyntheticEvent) {
    event.preventDefault();
    setBusy(true);
    setFailure(null);
    try {
      const created = await unwrap(
        client.POST("/api/v1/workspaces/{ws}/flows", {
          params: { path: { ws: workspaceId } },
          body: { name: name.trim(), settings: defaultSettings(trigger), graph: { v: 1, nodes: [], edges: [] } },
        }),
      );
      void navigate(`/w/${workspaceId}/flows/${created.id}`);
    } catch (error) {
      setFailure(describeError(error));
      setBusy(false);
    }
  }

  return (
    <div className="page">
      <PageHeader title="New flow" breadcrumb={{ group: "Messaging", section: "Flows", to: `/w/${workspaceId}/flows` }} lede="Name it and say what starts it. You lay out the steps next." />
      {ready && !canWrite ? (
        <Panel flush>
          <EmptyState title="You cannot create flows">Ask a member of the workspace to create one, or to give you a role.</EmptyState>
        </Panel>
      ) : (
        <form className="stack" onSubmit={(event) => void create(event)} noValidate>
          <Panel>
            <div className="stack">
              <TextField
                label="Name"
                value={name}
                onChange={(event) => setName(event.target.value)}
                maxLength={100}
                autoComplete="off"
                hint="Shown in the list of flows. It must differ from the other flows that are not archived."
              />
              <SelectField
                label="Starts when"
                value={trigger}
                onChange={(event) => setTrigger(event.target.value as TriggerType)}
                hint="You can change this, and say which segment or event, in the flow settings."
              >
                {TRIGGER_KINDS.map((kind) => (
                  <option key={kind.type} value={kind.type}>
                    {kind.label}
                  </option>
                ))}
              </SelectField>
            </div>
          </Panel>
          {failure ? <InlineError {...failure} /> : null}
          <div className="row">
            <Button type="submit" variant="primary" disabled={!ready || busy || name.trim() === ""}>
              {busy ? "Creating" : "Create flow"}
            </Button>
          </div>
        </form>
      )}
    </div>
  );
}
