import { createContext, useContext, useEffect, type ReactNode } from "react";
import { useMatch } from "react-router";

import { client, unwrap, useResource, type components, type Resource } from "../../api";

export type Workspace = components["schemas"]["WorkspaceBody"];

const STORAGE_KEY = "zirv-flow.workspace";

const WorkspacesContext = createContext<Resource<Workspace[]> | null>(null);

export function WorkspacesProvider({ children }: { children: ReactNode }) {
  const resource = useResource(() => unwrap(client.GET("/api/v1/workspaces")), []);
  return <WorkspacesContext.Provider value={resource}>{children}</WorkspacesContext.Provider>;
}

export function useWorkspaces(): Resource<Workspace[]> {
  const value = useContext(WorkspacesContext);
  if (!value) {
    throw new Error("useWorkspaces needs a WorkspacesProvider");
  }
  return value;
}

function remembered(): string | null {
  try {
    return globalThis.localStorage.getItem(STORAGE_KEY);
  } catch {
    return null;
  }
}

export function useCurrentWorkspace(): Workspace | null {
  const list = useWorkspaces();
  const match = useMatch("/w/:ws/*");
  const fromUrl = match?.params.ws ?? null;
  const all = list.status === "ready" ? list.data : [];
  const current =
    all.find((workspace) => workspace.id === fromUrl) ??
    (fromUrl === null ? all.find((workspace) => workspace.id === remembered()) : undefined) ??
    (fromUrl === null ? all[0] : undefined) ??
    null;

  const id = current?.id;
  useEffect(() => {
    if (id && fromUrl === id) {
      try {
        globalThis.localStorage.setItem(STORAGE_KEY, id);
      } catch {
        return;
      }
    }
  }, [id, fromUrl]);

  return current;
}
