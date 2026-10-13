import { useParams } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { EmptyState, PageHeader, Panel, ResourceView } from "../../ui";
import { useWorkspaceAccess } from "../members";
import { AiSettingsForm } from "./AiSettingsForm";
import { UsagePanel } from "./UsagePanel";

export function AiSettingsPage() {
  const { ws = "" } = useParams();
  const access = useWorkspaceAccess(ws);

  return (
    <div className="page">
      <PageHeader
        title="AI assistant"
        lede="Choose the model the assistant uses, what it may send to it, and how much it may spend. Every call is logged without its content."
      />
      <ResourceView resource={access} loadingLabel="Checking your access">
        {(data) =>
          data.canManage ? (
            <Settings key={ws} workspaceId={ws} />
          ) : (
            <Panel flush>
              <EmptyState title="You cannot manage the AI assistant here">
                Only workspace admins and the instance owner can see or change these settings. Ask an admin of this workspace.
              </EmptyState>
            </Panel>
          )
        }
      </ResourceView>
    </div>
  );
}

function Settings({ workspaceId }: { workspaceId: string }) {
  const models = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/ai/models", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );
  const settings = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/ai/settings", { params: { path: { ws: workspaceId } } })),
    [workspaceId],
  );

  return (
    <ResourceView
      resource={models}
      loadingLabel="Loading the available models"
      isEmpty={(data) => data.providers.length === 0}
      empty={
        <Panel flush>
          <EmptyState title="AI is not configured on this deployment">
            The server lists no model providers, so the assistant is unavailable. The operator adds providers and models
            in the server configuration.
          </EmptyState>
        </Panel>
      }
    >
      {(availableModels) => (
        <ResourceView resource={settings} loadingLabel="Loading the settings">
          {(current) => (
            <>
              <AiSettingsForm
                // A saved change loads fresh settings; remounting clears the key field.
                key={JSON.stringify(current)}
                workspaceId={workspaceId}
                models={availableModels}
                settings={current}
                onSaved={settings.reload}
              />
              <UsagePanel workspaceId={workspaceId} version={JSON.stringify(current)} />
            </>
          )}
        </ResourceView>
      )}
    </ResourceView>
  );
}
