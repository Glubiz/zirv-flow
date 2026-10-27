import { useParams } from "react-router";

import { client, unwrap, useResource } from "../../api";
import { ResourceView } from "../../ui";
import { useThemeAccess } from "./access";
import { ThemeEditor } from "./ThemeEditor";

export function ThemePage() {
  const { ws = "", id = "" } = useParams();
  const access = useThemeAccess(ws);
  const detail = useResource(
    () => unwrap(client.GET("/api/v1/workspaces/{ws}/themes/{id}", { params: { path: { ws, id } } })),
    [ws, id],
  );
  return (
    <ResourceView resource={access.ready ? detail : { status: "loading", reload: detail.reload }} loadingLabel="Loading the theme">
      {(data) => (
        <ThemeEditor
          key={`${ws}:${id}:${data.draft.revision}`}
          workspaceId={ws}
          initial={data}
          canWrite={access.canWrite}
          canAdmin={access.canAdmin}
          canPublish={access.canPublish}
          onReload={detail.reload}
        />
      )}
    </ResourceView>
  );
}
