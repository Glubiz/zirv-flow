import { useState } from "react";
import { Link, useParams } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { Button, EmptyState, PageHeader, Panel, Pill, ResourceView } from "../../ui";
import { formatDate } from "../../ui/format";
import { useTemplateAccess } from "./access";
import { NewTemplateDialog } from "./NewTemplateDialog";
import { statusOf } from "./model";

export function TemplatesPage() {
  const { ws = "" } = useParams();
  return <TemplateList key={ws} workspaceId={ws} />;
}

function TemplateList({ workspaceId }: { workspaceId: string }) {
  const access = useTemplateAccess(workspaceId);
  const [showArchived, setShowArchived] = useState(false);
  const [adding, setAdding] = useState(false);
  const templates = useResource(
    () =>
      unwrap(
        client.GET("/api/v1/workspaces/{ws}/templates", {
          params: { path: { ws: workspaceId }, query: { archived: showArchived } },
        }),
      ),
    [workspaceId, showArchived],
  );

  return (
    <div className="page">
      <PageHeader
        title="Templates"
        lede="The messages your flows send. Write one as HTML or MJML, see it rendered by the server as you type, then publish it for a send step to use."
        actions={
          access.canWrite ? (
            <Button variant="primary" onClick={() => setAdding(true)}>
              New template
            </Button>
          ) : undefined
        }
      />
      <div className="page-actions">
        <label className="check">
          <input type="checkbox" checked={showArchived} onChange={(event) => setShowArchived(event.target.checked)} />
          <span>Show archived templates</span>
        </label>
      </div>
      <Panel flush>
        <ResourceView
          resource={templates}
          loadingLabel="Loading templates"
          isEmpty={(data) => data.length === 0}
          empty={
            <EmptyState
              title="No templates yet"
              action={
                access.canWrite ? (
                  <Button variant="primary" onClick={() => setAdding(true)}>
                    New template
                  </Button>
                ) : undefined
              }
            >
              {access.canWrite ? "Make a template to use it in a send step." : "Members of the workspace make the templates."}
            </EmptyState>
          }
        >
          {(data) => (
            <div className="table-scroll">
              <table className="table">
                <caption>Templates of this workspace</caption>
                <thead>
                  <tr>
                    <th scope="col">Template</th>
                    <th scope="col">Channel</th>
                    <th scope="col">Category</th>
                    <th scope="col">Status</th>
                    <th scope="col">Changed</th>
                  </tr>
                </thead>
                <tbody>
                  {data.map((template) => {
                    const status = statusOf(template);
                    return (
                      <tr key={template.id}>
                        <td className="strong">
                          <Link to={`/w/${workspaceId}/templates/${template.id}`}>{template.name}</Link>
                        </td>
                        <td>{template.channel}</td>
                        <td>{template.category}</td>
                        <td>
                          <Pill tone={status.tone}>{status.label}</Pill>
                        </td>
                        <td>{formatDate(template.updated_at)}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
          )}
        </ResourceView>
      </Panel>
      {adding ? (
        <NewTemplateDialog
          workspaceId={workspaceId}
          onClose={() => setAdding(false)}
        />
      ) : null}
    </div>
  );
}
