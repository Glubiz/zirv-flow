import { useParams } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { ResourceView } from "../../ui";
import { useSnippetAccess } from "./access";
import { SnippetEditor } from "./SnippetEditor";

export function SnippetPage() {
  const { ws = "", id = "" } = useParams();
  const access = useSnippetAccess(ws);
  const detail = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/snippets/{id}", { params: { path: { ws, id } } })),
    [ws, id],
  );
  return (
    <ResourceView resource={access.ready ? detail : { status: "loading", reload: detail.reload }} loadingLabel="Loading the snippet">
      {(data) => (
        <SnippetEditor
          key={`${ws}:${id}:${data.draft.revision}`}
          workspaceId={ws}
          initial={data}
          canWrite={access.canWrite}
          canPublish={access.canPublish}
          onReload={detail.reload}
        />
      )}
    </ResourceView>
  );
}
