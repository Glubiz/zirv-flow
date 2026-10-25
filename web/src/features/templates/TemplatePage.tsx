import { useParams } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { ResourceView } from "../../ui";
import { useTemplateAccess } from "./access";
import { TemplateEditor } from "./TemplateEditor";

export function TemplatePage() {
  const { ws = "", id = "" } = useParams();
  const access = useTemplateAccess(ws);
  const detail = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/templates/{id}", { params: { path: { ws, id } } })),
    [ws, id],
  );
  return (
    <ResourceView resource={access.ready ? detail : { status: "loading", reload: detail.reload }} loadingLabel="Loading the template">
      {(data) => <TemplateEditor key={`${ws}:${id}`} workspaceId={ws} initial={data} canWrite={access.canWrite} canAdmin={access.canAdmin} />}
    </ResourceView>
  );
}
